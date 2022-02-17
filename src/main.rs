#![feature(let_else)]
#![feature(iter_intersperse)]

use std::collections::HashMap;

use anyhow::{Context as _, Result};
use clap::{Args as ClapArgs, Parser};
use healthchecks::model::NewCheck;
use k8s_openapi::api::batch::v1::CronJob;
use k8s_openapi::api::core::v1::EnvVar;
use kube::{Client, Config, ResourceExt};
use kube::api::{ListParams, PostParams};
use kube::config::{Kubeconfig, KubeConfigOptions};

#[derive(Parser, Debug)]
#[clap(name = "healthkube", version, author = "Jezza")]
struct Args {
	#[clap(flatten)]
	hc: HealthChecksInfo,

	/// Perform the synchronisation, but without making any altering calls.
	/// Note: Useful to find out what the program will end up deleting/creating/etc...
	#[clap(long)]
	dry_run: bool,

	/// The frequency at which a segment will be considered common enough to be used as a tag.
	#[clap(long, default_value_t = 2)]
	rank: u8,

	/// The corresponding kubernetes jobs will be updated with an environment variable that uses
	/// this argument as the key, and the HealthCheck id as the value.
	///
	/// For example using, --env_key MY_HC_KEY, will cause all kubernetes jobs to be updated to contain an env variable like so: "MY_HC_KEY = {the healthcheck id}"
	#[clap(long, env = "K8S_ENV_KEY")]
	env_key: Option<String>,

	/// Kubernetes contexts with namespaces.
	/// Pattern: context-name:namespace
	#[clap(required = true)]
	targets: Vec<String>,
}

#[derive(ClapArgs, Debug)]
#[clap(help_heading = "HEALTHCHECKS")]
struct HealthChecksInfo {
	/// The read/write healthchecks' api key.
	#[clap(long = "hc-key", env = "HC_API_KEY")]
	key: Option<String>,

	/// Where to find the Healthchecks instance.
	#[clap(long = "hc-url", env = "HC_API_URL")]
	url: Option<String>,

	/// Also known as channels.
	/// All of the integrations/channels to assign to all newly created checks.
	#[clap(long)]
	integrations: Vec<String>,

	/// Uses all integrations/channels currently registered for the project.
	#[clap(long, conflicts_with = "integrations")]
	all_integrations: bool,

	/// The timezone to use for all newly created checks.
	#[clap(long, default_value = "Europe/Berlin")]
	timezone: String,

	/// The expected period of this check in seconds.
	#[clap(long, default_value_t = 60 * 60 * 8)]
	timeout: i32,

	/// The grace period for this check in seconds.
	/// How long a check can be late before it's marked as errored.
	#[clap(long, default_value_t = 60 * 60 * 8)]
	grace: i32,

	/// Clears all existing checks.
	/// Note, it won't just remove those added.
	/// It will remove all of them. Completely.
	/// Without confirmation.
	#[clap(long)]
	clear_existing_checks: bool,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
	let Args {
		hc,
		dry_run,
		rank,
		env_key,
		targets,
	} = Parser::parse();

	let timezone = Some(hc.timezone);
	let timeout = Some(hc.timeout);
	let grace = Some(hc.grace);

	let hc_client = {
		let key = hc.key.context(
			"Unable to locate the HealthChecks API Key. [Try setting a env var: \"HC_API_KEY\"]",
		)?;
		let url = hc.url.context(
			"Unable to locate the HealthChecks API URL. [Try setting a env var: \"HC_API_URL\"]",
		)?;

		healthchecks::manage::get_client_with_url(key, None, url)
			.context("Unable to construct HealthChecks client")?
	};

	let integrations = {
		let mut integrations = hc.integrations;

		if hc.all_integrations {
			let all = hc_client
				.get_channels()?
				.into_iter()
				.map(|channel| channel.id);

			integrations.extend(all);
		}
		integrations.sort_unstable();
		integrations.dedup();

		let integrations = integrations.join(",");

		println!("Using integrations: {}", integrations);

		Some(integrations)
	};


	if !dry_run && hc.clear_existing_checks {
		hc_client
			.get_checks()?
			.into_iter()
			.filter_map(|check| check.id())
			.for_each(|id| {
				println!("Deleting: {}", id);
				hc_client
					.delete(&id)
					.context(format!("Unable to delete healthcheck: {}", id))
					.unwrap();
			});
	}

	let kubeconfig = Kubeconfig::read().unwrap();
	let mut opts = KubeConfigOptions::default();

	for target in targets {
		let (context, namespaces) = match target.split_once(':') {
			Some(values) => values,
			None => (&*target, "default"),
		};
		println!("Context: {}", context);
		opts.context = Some(context.into());

		let config = Config::from_custom_kubeconfig(kubeconfig.clone(), &opts)
			.await
			.unwrap();

		for namespace in namespaces.split(',') {
			println!("\tNamespace: {}", namespace);

			let kube_client = Client::try_from(config.clone()).unwrap();

			let kube_api: kube::Api<CronJob> = kube::Api::namespaced(kube_client, namespace);

			let jobs = kube_api.list(&ListParams::default()).await?.items;

			let common_tags = extract_tags(&jobs, rank).await;

			let register_job = |job: &CronJob| {
				let name = job.metadata.name.as_deref()?;
				let schedule = job.spec.as_ref().map(|d| &*d.schedule)?;

				let tags: String = name
					.split('-')
					.filter(|segment| common_tags.contains_key(*segment))
					.intersperse(" ")
					.collect();

				if dry_run {
					println!("\t\t: {: <50} -> [{}]", name, &tags);
					return None;
				}

				let new_check = NewCheck {
					name: Some(name.into()),
					schedule: Some(schedule.into()),
					tags: Some(tags),
					timeout,
					grace,
					tz: timezone.clone(),
					channels: integrations.clone(),
					unique: Some(vec![String::from("name")]),
					..Default::default()
				};

				let (_, check) = hc_client.upsert_check(new_check).ok()?;
				let id = check.id()?;
				println!("\t\t: {: <50} -> {}", name, id);
				Some(id)
			};

			let checks: Vec<String> = jobs.iter().filter_map(|job| register_job(job)).collect();

			let Some(env_key) = env_key.as_deref() else {
				continue;
			};

			let updated: Vec<_> = jobs
				.into_iter()
				.zip(checks.into_iter())
				.filter_map(|(mut job, check_id)| {
					let spec = job.spec.as_mut()?;
					let spec = spec.job_template.spec.as_mut()?;
					let spec = spec.template.spec.as_mut()?;

					for container in spec.containers.iter_mut() {
						let Some(env) = &mut container.env else {
							continue;
						};

						let index = env.iter().position(|env| env.name == env_key);

						let var = EnvVar {
							name: env_key.into(),
							value: Some(check_id.clone()),
							..EnvVar::default()
						};

						match index {
							Some(index) => env[index] = var,
							None => env.push(var),
						};
					}

					Some(job)
				})
				.collect();

			if dry_run {
				continue;
			}

			for job in updated {
				let params = PostParams::default();
				let _ = kube_api.replace(&job.name(), &params, &job).await?;
			}
		}
	}

	Ok(())
}

async fn extract_tags(jobs: &[CronJob], rank: u8) -> HashMap<&str, u8> {
	let mut common_tags: HashMap<&str, u8> = jobs
		.iter()
		.filter_map(|job| job.metadata.name.as_deref())
		.flat_map(|job| job.split('-'))
		.fold(HashMap::new(), |mut acc, item| {
			*acc.entry(item).or_default() += 1;
			acc
		});

	common_tags.remove(&"job");
	common_tags.retain(|_, v| *v > rank);

	common_tags
}
