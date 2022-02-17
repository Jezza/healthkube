
# Healthkube

A small program used to synchronise kubernetes cron jobs and healthcheck.io jobs.

## Installation

```shell
git clone https://github.com/Jezza/healthkube
cd healthkube
cargo install --path .
```

## Usage

Healthkube uses the local kubeconfig to query the cluster, and appropriate namespaces.
Here's a quick example to get you started:

```shell

healthkube --env-kube {} --all-integrations --hc-key {} --hc-url {} [TARGETS]

--env-key # Can be used to define a env key that will be updated with the healthchecks id.
--all-integrations # Used to fetch all healthcheck channels/integrations, and use them when creating/registering/updating new jobs.
--hc-key # Defines the Healthcheck api key (It uses the read/write api key.).
--hc-url # Where to find the healthcheck server.

TARGETS:
  Follows a simple pattern of "context", "context:namespace", or even "context:namespace1,namespace2,namespace3".
```

`healthkube --help` will give you a bigger look at all the flags.

