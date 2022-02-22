#![feature(let_else)]
#![feature(iter_intersperse)]
#![feature(vec_retain_mut)]

use std::collections::HashMap;

use anyhow::{Context as _, Result};
use clap::{Args as ClapArgs, Parser};
use healthchecks::model::NewCheck;
use k8s_openapi::api::batch::v1::CronJob;
use k8s_openapi::api::core::v1::Container;
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
	#[clap(long, default_value_t = 3)]
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
#[clap(next_help_heading = "HEALTHCHECKS")]
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
		let mut integrations = if hc.all_integrations {
			hc_client
				.get_channels()?
				.into_iter()
				.map(|channel| channel.id)
				.collect()
		} else {
			hc.integrations
		};

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

		let default_check = NewCheck {
			timeout,
			grace,
			tz: timezone.clone(),
			channels: integrations.clone(),
			..Default::default()
		};

		for namespace in namespaces.split(',') {
			println!("\tNamespace: {}", namespace);

			let kube_client = Client::try_from(config.clone()).unwrap();
			let kube_api: kube::Api<CronJob> = kube::Api::namespaced(kube_client, namespace);
			let mut jobs = kube_api.list(&ListParams::default()).await?.items;

			jobs.retain(|job| {
				if let Some(name) = &job.metadata.name {
					name == "sales-au-job-cleanup-shared-products-job"
				} else {
					false
				}
			});

			let definitions: Vec<_> = jobs.iter_mut()
				.filter_map(describe)
				.collect();

			let common_tags = {
				let mut common_tags: HashMap<&str, u8> = definitions.iter()
					.flat_map(|(name, _, _)| name.split('-'))
					.fold(HashMap::new(), |mut acc, item| {
						*acc.entry(item).or_default() += 1;
						acc
					});

				common_tags.remove("job");
				if rank > 0 {
					common_tags.retain(|_, v| *v >= rank);
				}

				common_tags
			};

			definitions.into_iter()
				.filter_map(|(name, schedule, containers)| {
					let tags: String = name
						.split('-')
						.filter(|segment| common_tags.contains_key(*segment))
						.intersperse(" ")
						.collect();

					if dry_run {
						println!("\t\t: {: <50} -> [{}]", name, &tags);
						return None;
					}

					let (status, check_id) = {
						let new_check = NewCheck {
							name: Some(name.into()),
							schedule: Some(schedule.into()),
							tags: Some(tags),
							unique: Some(vec![String::from("name")]),
							..default_check.clone()
						};

						let (status, check) = hc_client.upsert_check(new_check).ok()?;
						let check_id = check.id()?;

						let status = match status {
							healthchecks::manage::UpsertResult::Created => "Created",
							healthchecks::manage::UpsertResult::Updated => "Updated",
						};

						(status, check_id)
					};

					println!("\t\t: {: <50} -> {}(\"{}\")", name, status, check_id);

					let env_key = match env_key.as_deref() {
						Some(value) => value,
						None => {
							// Skip updating kubernetes, if no env_key was defined.
							containers.clear();
							return None;
						}
					};

					containers.retain_mut(|container| {
						let Some(env) = &mut container.env else {
							return false;
						};

						let item = env.iter_mut()
							.find(|env| env.name == env_key)
							.and_then(|var| var.value.as_mut());

						match item {
							Some(item) => {
								let need_to_update = *item != check_id;
								if need_to_update {
									*item = check_id.clone();
								}
								need_to_update
							}
							None => {
								let var = EnvVar {
									name: env_key.into(),
									value: Some(check_id.clone()),
									..Default::default()
								};
								env.push(var);
								true
							}
						}
					});

					if containers.is_empty() {
						return None;
					}

					Some(1)
				})
				.count();

			if dry_run {
				continue;
			}

			for mut job in jobs {
				let Some((_, _, containers)) = describe(&mut job) else {
					continue;
				};
				if containers.is_empty() {
					continue;
				}

				let params = PostParams::default();
				let _ = kube_api.replace(&job.name(), &params, &job).await?;
			}
		}
	}

	Ok(())
}

fn describe(job: &mut CronJob) -> Option<(&str, &str, &mut Vec<Container>)> {
	let CronJob {
		spec,
		metadata,
		..
	} = job;

	let name = metadata.name.as_deref()?;

	let spec = spec.as_mut()?;
	let schedule = &*spec.schedule;
	let spec = spec.job_template.spec.as_mut()?;
	let spec = spec.template.spec.as_mut()?;
	let containers = &mut spec.containers;

	Some((name, schedule, containers))
}
