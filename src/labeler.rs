use anyhow::{anyhow, bail};
use k8s_openapi::api::core::v1::Pod;
use kube::{
    api::{ListParams, Patch, PatchParams},
    Api, Client, ResourceExt,
};
use kube_derive::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::fmt::Debug;
use tracing::{error, trace};

use crate::metrics;

#[derive(CustomResource, Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[kube(group = "shiftflow.ml", version = "v1", kind = "Kdynaroll", namespaced)]
#[serde(rename_all = "camelCase")]
pub struct KdynarollConfigSpec {
    pub pod_labeler: PodLabelerSpec,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct PodLabelerSpec {
    pub namespaces: Vec<PodLabelerNamespacesSpec>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct PodLabelerNamespacesSpec {
    pub namespace: String,
    pub apps: Vec<PodLabelerAppSpec>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PodLabelerAppSpec {
    pub app: String, // the app name as defined in metadata.labels.app
    pub labels: Vec<PodLabelerLabelSpec>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PodLabelerLabelSpec {
    pub label_name: String,
    pub label_value_distribution: Vec<PodLabelerValueDistributionSpec>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct PodLabelerValueDistributionSpec {
    pub value: String,
    pub percentage: f64, // Use f64 for floating-point percentage
}

const ERROR_MSG_PERCENTAGE_SUM_NOT_100: &str = "The sum of all percentages must be exactly 100";

pub async fn apply_labels_to_app(client: &Client, namespace: &str, app: &str, applied_label_name: &str, label_distr: &Vec<PodLabelerValueDistributionSpec>) -> anyhow::Result<()> {
    let percentages_sum = label_distr
        .iter()
        .fold(0.0, |acc, value_distr| acc + value_distr.percentage);

    if percentages_sum != 100.0 {
        bail!(ERROR_MSG_PERCENTAGE_SUM_NOT_100);
    }
    let pod_api: Api<Pod> = Api::namespaced(client.clone(), namespace);

    let selector = "app=".to_string() + app;

    let list_params = ListParams::default().labels(&selector);

    match pod_api.list(&list_params).await {
        Ok(pods) => {
            trace!("Found {} pods for app {} in namespace {}", pods.items.len(), app, namespace);
            apply_labels_to_pods(client, namespace, &pods.items, applied_label_name, label_distr)
                .await
                .and_then(|resolved_pod_count| {
                    metrics::record_pod_count(namespace, app, resolved_pod_count as u64);
                    Ok(())
                })
        }
        Err(e) => Err(anyhow!("Failed to list pods for app {}: {:?}", app, e)),
    }
}

async fn apply_labels_to_pods(client: &Client, namespace: &str, pods: &Vec<Pod>, applied_label_name: &str, label_distr: &Vec<PodLabelerValueDistributionSpec>) -> anyhow::Result<usize> {
    let mut sorted_distr = label_distr.clone();

    // sort to make assigning highest frequency labels easier
    sorted_distr.sort_by(|lbl_dist_a, lbl_dist_b| {
        lbl_dist_a
            .percentage
            .partial_cmp(&lbl_dist_b.percentage)
            .unwrap()
            .reverse()
    });

    let pods_per_label = calculate_pods_per_label(&sorted_distr, pods.len());
    trace!("Pods per label: {:?}", pods_per_label);

    let resolved_result = resolve_pods_to_label(pods, pods_per_label, applied_label_name);

    match resolved_result {
        Ok(resolved_pods) => {
            let resolved_pod_count = resolved_pods.len();
            match assign_labels(client, namespace, applied_label_name, resolved_pods).await {
                Ok(_) => Ok(resolved_pod_count),
                Err(e) => Err(anyhow!("Failed to assign labels to pods: {:?}", e)),
            }
        }
        Err(e) => Err(anyhow!("Failed to resolve pods to labels: {:?}", e)),
    }
}

fn calculate_pods_per_label(sorted_distr: &Vec<PodLabelerValueDistributionSpec>, pod_count: usize) -> HashMap<String, usize> {
    let mut pods_per_label = HashMap::new();
    let mut total_pods = 0;

    if sorted_distr.is_empty() || pod_count == 0 {
        return pods_per_label;
    }

    let high_percentage_lbl = &sorted_distr[0].value;
    // here we expect sorted_distr to have been reverse sorted by percentage.
    // the sorted Vec<PodLabelerValueDistributionSpec> is also used outside of this function so we don't sort it here again to avoid
    // the unnecessary sort.
    // However, you'll notice that we add any missing pod will be added to the first label (highest percentage)
    // so if the sorted_distr is not sorted by percentage in reverse order, then any label will receive the missing pods

    for label_entry in sorted_distr {
        let pod_count_per_label = (pod_count as f64 * label_entry.percentage / 100.0).floor() as usize;

        // we've used all the available pods for other labels
        if total_pods >= pod_count {
            // no more pods available
            pods_per_label.insert(label_entry.value.clone(), 0);
        } else {
            total_pods += pod_count_per_label;
            pods_per_label.insert(label_entry.value.clone(), pod_count_per_label);
        }
    }

    // since we work with discrete number of pods, it's possible the sum percentages will not properly
    // ad up to total pods in the end so we need to assign the pods left over to a label.
    // we choose the first one, and given the expectation above, it's the one that has the highest percentage
    if total_pods < pod_count && pods_per_label.contains_key(high_percentage_lbl) {
        let diff = pod_count - total_pods;
        *pods_per_label.get_mut(high_percentage_lbl).unwrap() += diff;
    }

    pods_per_label
}

fn resolve_pods_to_label(pods: &Vec<Pod>, mut pods_per_label: HashMap<String, usize>, applied_label_name: &str) -> anyhow::Result<HashMap<String, String>> {
    const LABEL_PLACEHOLDER: String = String::new();

    let mut resolved_pods = HashMap::new();

    let total_labels = pods_per_label.iter().fold(0, |acc, (_, count)| acc + count);
    if total_labels != pods.len() {
        bail!("The total number of labels {} does not match the number of pods {}", total_labels, pods.len());
    }

    for pod in pods {
        if let Some(pod_name) = pod.metadata.name.clone() {
            let must_add_label = pod
                .labels()
                .get(applied_label_name)
                .and_then(|pod_label| pods_per_label.get_mut(pod_label))
                .map(|count| {
                    if *count > 0 {
                        *count -= 1;
                        return false;
                    }
                    return true;
                })
                .unwrap_or(true);

            if must_add_label {
                resolved_pods.insert(pod_name.to_string(), LABEL_PLACEHOLDER);
            }
        } else {
            error!("Pod has no name and this shouldn't be possible. Pod {:?}", pod);
            bail!("Found pod without a name");
        }
    }

    let num_labels_to_add = pods_per_label.values().fold(0, |acc, val| val + acc);
    if num_labels_to_add != resolved_pods.len() {
        bail!(
            "The total number of labels {} does not match the number of pods {}. This should never happen and yet, it did.",
            num_labels_to_add,
            resolved_pods.len()
        );
    }

    let mut resolved_pods_iter = resolved_pods.iter_mut();
    for (label, num_pods) in pods_per_label {
        for _ in 0..num_pods {
            if let Some((_, pod_label)) = resolved_pods_iter.next() {
                *pod_label = label.clone();
            }
        }
    }

    return Ok(resolved_pods);
}

async fn assign_labels(client: &Client, namespace: &str, applied_label_name: &str, resolved_pod_labels: HashMap<String, String>) -> anyhow::Result<()> {
    for (pod_name, label_value) in resolved_pod_labels {
        let assign_result = assign_label_to_pod(client, namespace, applied_label_name, pod_name, label_value).await;
        if assign_result.is_err() {
            return assign_result;
        }
    }

    Ok(())
}
async fn assign_label_to_pod(client: &Client, namespace: &str, applied_label_name: &str, pod_name: String, label_value: String) -> anyhow::Result<()> {
    let label_patch = json!({
        "metadata": {
            "labels": {
                applied_label_name: label_value
            }
        }
    });

    let patch_data = Patch::Merge(&label_patch);

    let podapi: Api<Pod> = Api::namespaced(client.clone(), namespace);
    let patch_params = PatchParams::apply("kdynaroll_patch_call");
    let patched_pod = podapi
        .patch(pod_name.as_str(), &patch_params, &patch_data)
        .await;

    patched_pod
        .inspect(|_| trace!("Pod {} assigned label {} with value {}", pod_name, applied_label_name, label_value))
        .map_err(|pod_error| anyhow::anyhow!("Error labeling pod: {:?}", pod_error))
        .map(|_| ())
}

mod test_pod_and_label {
    #[cfg(test)]
    mod tests {
        use super::super::*;
        use kube::api::ObjectMeta;
        use std::collections::BTreeMap;

        const KDYNAROLL_LABEL_NAME: &str = "kdynaroll_label";

        #[test]
        fn test_distribute_pods() {
            let distr = vec![
                PodLabelerValueDistributionSpec {
                    value: "A".to_string(),
                    percentage: 97.0,
                },
                PodLabelerValueDistributionSpec {
                    value: "B".to_string(),
                    percentage: 2.0,
                },
                PodLabelerValueDistributionSpec {
                    value: "C".to_string(),
                    percentage: 1.0,
                },
            ];

            let pod_len = 5;
            let assignments = calculate_pods_per_label(&distr, pod_len);

            assert_eq!(
                assignments,
                HashMap::from([
                    ("A".to_string(), 5),
                    ("B".to_string(), 0),
                    ("C".to_string(), 0),
                ])
            );
        }

        #[test]
        fn test_distribute_pods_empty_percentages() {
            let distr = vec![];
            let pod_len = 500;
            let assignments = calculate_pods_per_label(&distr, pod_len);

            assert!(assignments.is_empty());
        }

        #[test]
        fn test_distribute_pods_no_pods() {
            let distr = vec![PodLabelerValueDistributionSpec {
                value: "onlyone".to_string(),
                percentage: 100.0,
            }];

            let pod_len = 0;
            let assignments = calculate_pods_per_label(&distr, pod_len);

            assert!(assignments.is_empty());
        }

        #[test]
        fn test_distributed_pods_even_split() {
            let distr = vec![
                PodLabelerValueDistributionSpec {
                    value: "A".to_string(),
                    percentage: 50.0,
                },
                PodLabelerValueDistributionSpec {
                    value: "B".to_string(),
                    percentage: 50.0,
                },
            ];

            let pod_len = 10;
            let assignments = calculate_pods_per_label(&distr, pod_len);

            assert_eq!(assignments, HashMap::from([("A".to_string(), 5), ("B".to_string(), 5),]));
        }

        #[test]
        fn test_resolve_pods_no_labels_in_meta() {
            let pods = make_test_pods(2, 0);
            let pods_per_label = HashMap::from([("labelA".to_string(), 1usize), ("labelB".to_string(), 1)]);

            let expected_labeled_pods = pods
                .iter()
                .map(|pod| pod.metadata.name.as_ref().unwrap().to_string())
                .collect::<Vec<String>>();

            verify_resolve_pods_label_all(&pods, pods_per_label.clone(), expected_labeled_pods, pods_per_label.clone());
        }

        #[test]
        fn test_resolve_pods_with_other_labels_in_meta() {
            // create pods with 3 labels named "anylabel"
            let pods = make_test_pods(4, 3);
            let pods_per_label = HashMap::from([("label1".to_string(), 3usize), ("label2".to_string(), 1)]);

            let expected_labeled_pods = pods
                .iter()
                .map(|pod| pod.metadata.name.as_ref().unwrap().to_string())
                .collect::<Vec<String>>();

            verify_resolve_pods_label_all(&pods, pods_per_label.clone(), expected_labeled_pods, pods_per_label.clone());
        }

        #[test]
        fn test_resolve_pods_some_already_labeled_all_labels_applied() {
            // create pods with labels named "anylabel"
            let mut pods = make_test_pods(5, 1);
            let pods_per_label = HashMap::from([("label1".to_string(), 3usize), ("label2".to_string(), 2)]);

            // change 2 pods to already have the labels we want
            pods[0].metadata.labels = Some(BTreeMap::from([(KDYNAROLL_LABEL_NAME.to_string(), "label1".to_string())]));

            pods[2].metadata.labels = Some(BTreeMap::from([(KDYNAROLL_LABEL_NAME.to_string(), "label2".to_string())]));

            // expected_labeled_pods are the pods that don't have KDYNAROLL_LABEL_NAME as a label
            let expected_labeled_pods = pods
                .iter()
                .filter(|pod| {
                    pod.metadata
                        .labels
                        .as_ref()
                        .unwrap()
                        .get(KDYNAROLL_LABEL_NAME)
                        .is_none()
                })
                .map(|pod| pod.metadata.name.as_ref().unwrap().to_string())
                .collect::<Vec<String>>();

            let expected_label_pod_count = HashMap::from([("label1".to_string(), 2usize), ("label2".to_string(), 1)]);

            verify_resolve_pods_label_all(&pods, pods_per_label.clone(), expected_labeled_pods, expected_label_pod_count);
        }

        #[test]
        fn test_resolve_pods_some_already_labeled_some_labels_applied() {
            const UNLABELED_POD_COUNT: usize = 5;
            // create pods with labels named "anylabel"
            let mut pods = make_test_pods(11, 1);
            let pods_per_label = HashMap::from([
                ("label1".to_string(), UNLABELED_POD_COUNT),
                ("label2".to_string(), 2),
                ("label3".to_string(), 4),
            ]);

            // fill all label1 pods
            for idx in 0..UNLABELED_POD_COUNT {
                pods[idx].metadata.labels = Some(BTreeMap::from([(KDYNAROLL_LABEL_NAME.to_string(), "label1".to_string())]));
            }

            // collect all pods skipping the first 5
            let expected_labeled_pods = pods
                .iter()
                .skip(UNLABELED_POD_COUNT)
                .map(|pod| pod.metadata.name.as_ref().unwrap().to_string())
                .collect::<Vec<String>>();

            let expected_label_pod_count = HashMap::from([("label2".to_string(), 2usize), ("label3".to_string(), 4)]);

            verify_resolve_pods_label_all(&pods, pods_per_label.clone(), expected_labeled_pods, expected_label_pod_count);
        }

        #[test]
        fn test_resolve_pods_all_already_labeled_multiple_labels() {
            let mut pods = make_test_pods(8, 1);
            let pods_per_label = HashMap::from([
                ("label1".to_string(), 2),
                ("label2".to_string(), 2),
                ("label3".to_string(), 2),
                ("label4".to_string(), 2),
            ]);

            // fill all label1 pods
            for idx in 0..pods.len() {
                let label_value = format!("label{}", idx / 2 + 1);
                pods[idx].metadata.labels = Some(BTreeMap::from([(KDYNAROLL_LABEL_NAME.to_string(), label_value)]));
            }

            verify_resolve_pods_label_all(&pods, pods_per_label.clone(), vec![], HashMap::new());
        }

        #[test]
        fn test_resolve_pods_some_already_labeled_new_label() {
            let mut pods = make_test_pods(6, 2);
            let pods_per_label = HashMap::from([
                ("label1".to_string(), 1),
                ("label2".to_string(), 3),
                ("label3".to_string(), 2),
            ]);

            pods[0].metadata.labels = Some(BTreeMap::from([(KDYNAROLL_LABEL_NAME.to_string(), "label1".to_string())]));

            pods[1].metadata.labels = Some(BTreeMap::from([(KDYNAROLL_LABEL_NAME.to_string(), "label2".to_string())]));

            let expected_labeled_pods = pods
                .iter()
                .skip(2)
                .map(|pod| pod.metadata.name.as_ref().unwrap().to_string())
                .collect::<Vec<String>>();

            let expected_label_pod_count = HashMap::from([("label2".to_string(), 2usize), ("label3".to_string(), 2)]);

            verify_resolve_pods_label_all(&pods, pods_per_label.clone(), expected_labeled_pods, expected_label_pod_count);
        }

        #[test]
        fn test_resolve_pods_all_already_labeled_new_label() {
            let mut pods = make_test_pods(2, 1);
            let pods_per_label = HashMap::from([("label1".to_string(), 1), ("label4".to_string(), 1)]);

            pods[0].metadata.labels = Some(BTreeMap::from([(KDYNAROLL_LABEL_NAME.to_string(), "label1".to_string())]));

            pods[1].metadata.labels = Some(BTreeMap::from([(KDYNAROLL_LABEL_NAME.to_string(), "label2".to_string())]));

            let expected_labeled_pods = vec!["pod2".to_string()];

            let expected_label_pod_count = HashMap::from([("label4".to_string(), 1)]);

            verify_resolve_pods_label_all(&pods, pods_per_label.clone(), expected_labeled_pods, expected_label_pod_count);
        }

        #[test]
        fn test_resolve_pods_some_already_labeled_remove_label() {
            let mut pods = make_test_pods(3, 1);
            let pods_per_label = HashMap::from([
                ("label1".to_string(), 1),
                ("label2".to_string(), 1),
                ("label3".to_string(), 1),
            ]);

            pods[0].metadata.labels = Some(BTreeMap::from([(KDYNAROLL_LABEL_NAME.to_string(), "label7".to_string())]));

            pods[1].metadata.labels = Some(BTreeMap::from([(KDYNAROLL_LABEL_NAME.to_string(), "label5".to_string())]));

            pods[2].metadata.labels = Some(BTreeMap::from([(KDYNAROLL_LABEL_NAME.to_string(), "label6".to_string())]));

            let expected_labeled_pods = vec!["pod1".to_string(), "pod2".to_string(), "pod3".to_string()];

            let expected_label_pod_count = HashMap::from([
                ("label1".to_string(), 1usize),
                ("label2".to_string(), 1),
                ("label3".to_string(), 1),
            ]);

            verify_resolve_pods_label_all(&pods, pods_per_label.clone(), expected_labeled_pods, expected_label_pod_count);
        }

        #[test]
        fn test_resolve_pods_more_than_needed_already_labeled() {
            let mut pods = make_test_pods(5, 1);
            let pods_per_label = HashMap::from([
                ("label1".to_string(), 2),
                ("label2".to_string(), 1),
                ("label3".to_string(), 2),
            ]);

            for idx in 0..pods.len() {
                let label_value = format!("label{}", idx / 3 + 1);
                pods[idx].metadata.labels = Some(BTreeMap::from([(KDYNAROLL_LABEL_NAME.to_string(), label_value)]));
            }

            let expected_labeled_pods = vec!["pod3".to_string(), "pod5".to_string()];

            let expected_label_pod_count = HashMap::from([("label3".to_string(), 2)]);

            verify_resolve_pods_label_all(&pods, pods_per_label.clone(), expected_labeled_pods, expected_label_pod_count);
        }

        #[test]
        fn test_fail_if_fewer_labels_than_pods() {
            let pods = make_test_pods(5, 0);
            let pods_per_label = HashMap::from([("label1".to_string(), 1usize), ("label2".to_string(), 2)]);

            let resolved_pods = resolve_pods_to_label(&pods, pods_per_label, KDYNAROLL_LABEL_NAME);
            assert!(resolved_pods.is_err());
        }

        #[test]
        fn test_fail_if_more_labels_than_pods() {
            let pods = make_test_pods(2, 0);
            let pods_per_label = HashMap::from([("label1".to_string(), 7usize)]);

            let resolved_pods = resolve_pods_to_label(&pods, pods_per_label, KDYNAROLL_LABEL_NAME);
            assert!(resolved_pods.is_err());
        }

        fn verify_resolve_pods_label_all(pods: &Vec<Pod>, pods_per_label: HashMap<String, usize>, expected_labeled_pods: Vec<String>, expected_labeled_pod_count: HashMap<String, usize>) {
            let resolved_pods = resolve_pods_to_label(&pods, pods_per_label, KDYNAROLL_LABEL_NAME).unwrap();

            assert_eq!(resolved_pods.len(), expected_labeled_pods.len());

            // check all pods are covered
            for labeled_pod in &expected_labeled_pods {
                assert!(resolved_pods.contains_key(labeled_pod));
            }

            let mut actual_labeled_pod_count = HashMap::new();
            for (_, label) in resolved_pods {
                actual_labeled_pod_count
                    .entry(label)
                    .and_modify(|count| *count += 1)
                    .or_insert(1);
            }

            assert_eq!(actual_labeled_pod_count, expected_labeled_pod_count);
        }

        fn make_test_pods(count: u16, label_count: u16) -> Vec<Pod> {
            let mut pods = Vec::new();
            let mut labels = BTreeMap::new();

            for i in 1..label_count + 1 {
                labels.insert(format!("anylabel{}", i), "value".to_string());
            }

            let label_data = match label_count {
                0 => None,
                _ => Some(labels),
            };

            for i in 1..count + 1 {
                let mut pod = Pod::default();
                pod.metadata = ObjectMeta {
                    name: Some(format!("pod{}", i)),
                    labels: label_data.clone(),
                    ..ObjectMeta::default()
                };
                pods.push(pod);
            }

            pods
        }
    }
}

mod test_apply_labels_to_app {
    #[cfg(test)]
    mod tests {
        use super::super::*;

        #[tokio::test]
        async fn test_apply_labels_to_app_percentages_not_equals_100() {
            let label_dist: Vec<PodLabelerValueDistributionSpec> = vec![
                PodLabelerValueDistributionSpec {
                    value: "label50".to_string(),
                    percentage: 50.0,
                },
                PodLabelerValueDistributionSpec {
                    value: "label49.9".to_string(),
                    percentage: 49.9,
                },
            ];

            let client = test_mocks::make_mock_client_noop();

            let result = apply_labels_to_app(&client, "default", "app", "the-label-name", &label_dist).await;

            let expected_err_msg = ERROR_MSG_PERCENTAGE_SUM_NOT_100;
            let result_msg = result.unwrap_err().to_string();
            assert_eq!(result_msg, expected_err_msg);
        }
    }
}

mod test_assign_labels {
    #[cfg(test)]
    mod tests {
        use super::super::*;
        use futures::executor::block_on;
        use std::{
            collections::HashSet,
            sync::{Arc, Mutex},
        };

        #[derive(PartialEq, Eq, Hash, Debug)]
        struct RequestResults {
            uri: String,
            body: String,
            method: http::Method,
        }

        #[tokio::test]
        async fn test_assign_labels_success() {
            let patch_pod1_uri: String = "/api/v1/namespaces/default/pods/pod1?&fieldManager=kdynaroll_patch_call".to_string();
            let patch_pod2_uri: String = "/api/v1/namespaces/default/pods/pod2?&fieldManager=kdynaroll_patch_call".to_string();

            let pod = Pod {
                ..Default::default()
            };
            let pod_response_content = serde_json::json!(pod);
            let response_content = HashMap::from([
                (patch_pod1_uri.clone(), pod_response_content.clone()),
                (patch_pod2_uri.clone(), pod_response_content),
            ]);

            let request_results: Arc<Mutex<HashSet<RequestResults>>> = Arc::new(Mutex::new(HashSet::new()));
            let request_results_for_async = request_results.clone();

            let on_request_received = move |request: http::Request<kube_client::client::Body>| {
                let request_uri = request.uri().path_and_query().unwrap().to_string();
                let request_method = request.method().clone();
                let req_body_bytes = request.into_body().collect_bytes();

                let req_result = RequestResults {
                    uri: request_uri,
                    body: String::from_utf8(block_on(async { req_body_bytes.await }).unwrap().to_vec()).unwrap(),
                    method: request_method,
                };

                request_results_for_async.lock().unwrap().insert(req_result);
            };

            let pod_labels = HashMap::from([
                ("pod1".to_string(), "label-valueA".to_string()),
                ("pod2".to_string(), "label-valueB".to_string()),
            ]);

            let (spawned, service) = test_mocks::make_mock_kube_client(response_content, on_request_received, pod_labels.len() as i32);

            let client = Client::new(service, "default".to_string());

            let assign_result = assign_labels(&client, "default", "labelname", pod_labels).await;
            assert!(assign_result.is_ok());

            spawned.await.unwrap();

            let expected_results = HashSet::from([
                RequestResults {
                    uri: patch_pod1_uri,
                    body: "{\"metadata\":{\"labels\":{\"labelname\":\"label-valueA\"}}}".to_string(),
                    method: http::Method::PATCH,
                },
                RequestResults {
                    uri: patch_pod2_uri,
                    body: "{\"metadata\":{\"labels\":{\"labelname\":\"label-valueB\"}}}".to_string(),
                    method: http::Method::PATCH,
                },
            ]);

            let actual_results = request_results.lock().unwrap();
            assert_eq!(*actual_results, expected_results);
        }
    }
}
