# KDynaRoll
A Kubernetes Controller for dynamically managing application behavior through labels.

## Features

- Assigns labels to pods based on percentage distributions.
- Supports multiple labels per pod.
- Automatically maintains label distribution as the number of pods changes.
- Exposes controller metrics for Prometheus-compatible scrapers.


## Applications

KDynaRoll was originally created to address a very particular use case, combining pod labels and feature flags (for use with tools like Unleash, Flagsmith, Harness, etc).

This is a slightly more generic version of the original use case and it hasn't been stress tested as much, hence the **beta status**.

I'd very much like to hear about other cases and requirements you may have. Open an [issue](https://github.com/bignacio/kdynaroll/issues) I'll check it out!

Here are some other applications for KDynaRoll:

- A/B testing or canary deployments:
- Resource segmentation: dynamically assign pods to resource groups based on specific needs or quotas, then schedule them differently based on these labels.
- Dynamically change internal or external ingress access to services
- Fault Tolerance Testing: assign failure-injection labels to simulate real-world failure scenarios
- Cost Optimization: use labels to represent different classes of service (e.g., spot-instances, reserved-instances), helping allocate cheaper resources to less critical pods.
- Security/Compliance Segmentation: In multi-tenant or compliance-focused environments, apply different policies through labels.


The [Kubernetes Downward API](https://kubernetes.io/docs/concepts/workloads/pods/downward-api/) can be used to expose pod labels to applications.

## Using a docker image

Images are available on **dockerhub** under biraignacio/kdynaroll.

```
docker pull biraignacio/kdynaroll
```

It's heavily rate limited so consider building from source and and deploying to your local docker repository.


## Building


To build from source, you need a Rust compiler with at least `edition 2021`.
After building the binary, create and publish a Docker image to your repository.

```
cargo build --release

# specify the repository and tag
docker build -t <my.repository>:<port>/kdynaroll:<tag> target/release/ -f Dockerfile

docker push <my.repository>:<port>/kdynaroll:<tag>
```

## Getting started


### Installation

Install KDynaRoll using the provided [helm chart](charts).
Configure KDynaRoll via environment variables, defined in [values.yaml](charts/kdynaroll/values.yaml).

```yaml
kdynaroll:
  env:
    logLevel: "INFO" # log level
    metricsBindIpAddr: "0.0.0.0" # address the metrics server will listen to
    metricsBindPort: 5085 # metrics server port
```

You should also specify the docker repository and tag  for pulling the KDynaRoll image. (See build instructions above)
```
image:
  repository: my.docker.registry:5000/kdynaroll
  tag: v0.1.0
```

A new Custom Resource Definition (CRD) named `kdynarolls.shiftflow.ml` will then be created.


### Specifying targets labels to be applied

After installation, create a KDynaRoll resource to specify the target applications where the labels will be applied. Labels don't need to be predefined.

Label name is, well the name of the label. A label can have multiple values, distributed across pods using the defined percentages.

KDynaRoll supports multiple namespaces, applications, and labels. Each label's values must have percentages between 0 and 100, with their total adding up to 100%.

A KDynaRoll definition has the following structure:

```yaml
apiVersion: shiftflow.ml/v1
kind: Kdynaroll
metadata:
  name: kdynaroll-controller-config
  namespace: <your namespace>
spec:
  podLabeler:
    namespaces: # list of namespaces
    - namespace: <namespace>
      apps: # list of apps
      - app: <app name>
        labels: # list of labels to be applied
        - labelName: <label name>
          labelValueDistribution: # list of possible values for this label
          - value: <label value 1>
            percentage: <percentage for this value as a floating point > 0 and <= 100.0>
```

Make sure the name of the configuration (in `metadata.name`) is `kdynaroll-controller-config`. For the moment, that's a requirement.
```yaml
metadata:
  name: kdynaroll-controller-config
```

KDynaRoll currently applies only to applications with the label app. Ensure your target applications include this label.


See [loadbalancer](examples/loadbalancer/crd-config.yaml) for an example CRD config file.


## FAQ

### What is the best way to make coffee?

It's a matter of personal preference. What works for you is the best method.


### Why Rust?

A: Personally, I prefer French press coffee.