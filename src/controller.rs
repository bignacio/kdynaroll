use futures::StreamExt;
use kube::{Api, Client};
use kube_runtime::controller::{Action, Controller};
use kube_runtime::watcher::Config;
use std::sync::Arc;
use tokio::time::Duration;
use tracing::{error, info, trace};

use crate::labeler::{self, Kdynaroll, KdynarollConfigSpec, PodLabelerNamespacesSpec};
use crate::metrics::{self};
use crate::task_runtime::CONTROLLER_TASK_RUNTIME;

pub async fn start_controller(client: Client) -> Result<(), kube::Error> {
    let controller_namespace = std::env::var("KDYNAROLL_POD_NAMESPACE").unwrap_or("default".to_string());

    let crd_api: Api<Kdynaroll> = Api::namespaced(client.clone(), &controller_namespace);

    let selector = "metadata.name=kdynaroll-controller-config".to_string();

    let cfg = Config::default().fields(&selector);
    Controller::new(crd_api, cfg)
        .run(reconcile, error_policy, Arc::new(client.clone()))
        .for_each(|res| async {
            match res {
                Ok(o) => trace!("Reconciled {:?}", o),
                Err(e) => error!("Reconcile failed: {:?}", e),
            }
        })
        .await;

    Ok(())
}

async fn reconcile(kdn: Arc<Kdynaroll>, client: Arc<Client>) -> Result<Action, kube::Error> {
    trace!("Reconciling controller: {:?}", kdn);

    metrics::record_reconcile_cycle("success");
    let start_time = std::time::Instant::now();
    scan_namespaces(&client, &kdn.spec).await;

    let elapsed_time = start_time.elapsed().as_millis() as f64;
    metrics::record_reconcile_time(elapsed_time);

    info!("Namespace scan completed");

    Ok(Action::requeue(Duration::from_secs(300)))
}

fn error_policy(kdn: Arc<Kdynaroll>, error: &kube::Error, _: Arc<Client>) -> Action {
    error!("Error {:?} reconciling controller: {:?}", error, kdn);
    metrics::record_reconcile_cycle("error");

    Action::requeue(Duration::from_secs(300))
}

async fn scan_namespaces(client: &Client, ctrl_config: &KdynarollConfigSpec) {
    let mut ns_futures = Vec::new();

    let ns_count = ctrl_config.pod_labeler.namespaces.len() as u64;
    metrics::record_namespace_count(ns_count);

    for namespace_spec in ctrl_config.pod_labeler.namespaces.clone() {
        let task_client = client.clone();

        let ns_future = CONTROLLER_TASK_RUNTIME.spawn(async move { scan_apps(task_client, namespace_spec).await });

        ns_futures.push(ns_future);
    }

    for result in futures::future::join_all(ns_futures).await {
        match result {
            Ok(namespace_scan_result) => match namespace_scan_result {
                Ok(_) => (),
                Err(scan_error) => error!("{}", scan_error),
            },
            Err(join_error) => error!("{}", join_error),
        }
    }
}

async fn scan_apps(client: Client, namespace_spec: PodLabelerNamespacesSpec) -> anyhow::Result<()> {
    let namespace = &namespace_spec.namespace;
    trace!("Scanning namespace {}", namespace);

    let app_count = namespace_spec.apps.len() as u64;
    metrics::record_app_count(namespace, app_count);

    for apps_spec in namespace_spec.apps {
        let app = &apps_spec.app;

        for labels_spec in apps_spec.labels {
            let applied_label_name = &labels_spec.label_name;
            let label_dist = &labels_spec.label_value_distribution;

            match labeler::apply_labels_to_app(&client, namespace, app, applied_label_name, label_dist).await {
                Ok(_) => info!("Applied labels to app {}", app),
                Err(kerror) => error!("Failed to apply labels to app {}: {:?}", app, kerror),
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::labeler::*;
    use super::*;
    use futures::executor::block_on;
    use k8s_openapi::api::core::v1::Pod;
    use kube::api::ObjectList;
    use kube::api::ObjectMeta;
    use kube::Client;
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(PartialEq, Eq, Hash, Debug)]
    struct RequestContent {
        body: String,
        method: http::Method,
    }

    #[tokio::test]
    async fn test_full_reconcile() {
        let (response_content, num_calls) = make_full_reconcile_response_content();

        let requests_content: Arc<Mutex<HashMap<String, RequestContent>>> = Arc::new(Mutex::new(HashMap::new()));
        let requests_content_for_async = requests_content.clone();

        let on_request_received = move |request: http::Request<kube_client::client::Body>| {
            let request_uri = request.uri().path_and_query().unwrap().to_string();
            let request_method = request.method().clone();
            let req_body_bytes = request.into_body().collect_bytes();

            let content = RequestContent {
                body: String::from_utf8(block_on(async { req_body_bytes.await }).unwrap().to_vec()).unwrap(),
                method: request_method,
            };

            requests_content_for_async
                .lock()
                .unwrap()
                .insert(request_uri, content);
        };

        let (spawned, service) = test_mocks::make_mock_kube_client(response_content, on_request_received, num_calls);
        let client = Arc::new(Client::new(service, "default"));

        let kdynaroll_cfg = Arc::new(Kdynaroll {
            metadata: ObjectMeta {
                name: Some("kdynaroll-controller-config".to_string()),
                ..Default::default()
            },
            spec: make_full_reconcile_kdynaroll_test_spec(),
        });

        let reconcile_result = reconcile(kdynaroll_cfg, client).await;
        spawned.await.unwrap();

        let expected_action_result = Action::requeue(Duration::from_secs(300));
        let actual_action_result = reconcile_result.expect("reconcile should not return error");

        assert_eq!(expected_action_result, actual_action_result);

        let expected_responses = make_full_reconcile_expected_response();
        let actual_responses = requests_content.lock().unwrap();

        assert_eq!(actual_responses.len(), num_calls as usize);
        assert_eq!(expected_responses, *actual_responses);
    }

    fn make_full_reconcile_kdynaroll_test_spec() -> KdynarollConfigSpec {
        KdynarollConfigSpec {
            pod_labeler: PodLabelerSpec {
                namespaces: vec![
                    PodLabelerNamespacesSpec {
                        namespace: "ns1".to_string(),
                        apps: vec![
                            PodLabelerAppSpec {
                                app: "app1".to_string(),
                                labels: vec![PodLabelerLabelSpec {
                                    label_name: "label1app1".to_string(),
                                    label_value_distribution: vec![PodLabelerValueDistributionSpec {
                                        value: "label1app1value".to_string(),
                                        percentage: 100.0,
                                    }],
                                }],
                            },
                            PodLabelerAppSpec {
                                app: "app2".to_string(),
                                labels: vec![PodLabelerLabelSpec {
                                    label_name: "label2".to_string(),
                                    label_value_distribution: vec![PodLabelerValueDistributionSpec {
                                        value: "label2app2value".to_string(),
                                        percentage: 100.0,
                                    }],
                                }],
                            },
                        ],
                    },
                    PodLabelerNamespacesSpec {
                        namespace: "ns2".to_string(),
                        apps: vec![
                            PodLabelerAppSpec {
                                app: "app3".to_string(),
                                labels: vec![PodLabelerLabelSpec {
                                    label_name: "label3".to_string(),
                                    label_value_distribution: vec![PodLabelerValueDistributionSpec {
                                        value: "label3app3value".to_string(),
                                        percentage: 100.0,
                                    }],
                                }],
                            },
                            PodLabelerAppSpec {
                                app: "app4".to_string(),
                                labels: vec![PodLabelerLabelSpec {
                                    label_name: "label4".to_string(),
                                    label_value_distribution: vec![PodLabelerValueDistributionSpec {
                                        value: "label4app4value".to_string(),
                                        percentage: 100.0,
                                    }],
                                }],
                            },
                        ],
                    },
                ],
            },
        }
    }

    fn make_full_reconcile_response_content() -> (HashMap<String, serde_json::Value>, i32) {
        let fn_make_pod = |pod_name: &str| Pod {
            metadata: ObjectMeta {
                name: Some(pod_name.to_string()),
                ..Default::default()
            },
            ..Default::default()
        };

        let pod_list_ns1app1response = serde_json::json!(ObjectList::<Pod> {
            items: vec![fn_make_pod("ns1app1pod1"), fn_make_pod("ns1app1pod2")],
            metadata: Default::default(),
            types: Default::default(),
        });

        let pod_list_ns1app2response = serde_json::json!(ObjectList::<Pod> {
            items: vec![fn_make_pod("ns1app2pod3"), fn_make_pod("ns1app2pod4")],
            metadata: Default::default(),
            types: Default::default(),
        });

        let pod_list_ns2app3response = serde_json::json!(ObjectList::<Pod> {
            items: vec![fn_make_pod("ns2app3pod5"), fn_make_pod("ns2app3pod6")],
            metadata: Default::default(),
            types: Default::default(),
        });

        let pod_list_ns2app4response = serde_json::json!(ObjectList::<Pod> {
            items: vec![fn_make_pod("ns2app4pod7"), fn_make_pod("ns2app4pod8")],
            metadata: Default::default(),
            types: Default::default(),
        });

        let pod_patch_response = serde_json::json!(Pod {
            ..Default::default()
        });

        let response_content = HashMap::from([
            ("/api/v1/namespaces/ns1/pods?&labelSelector=app%3Dapp1".to_string(), pod_list_ns1app1response),
            ("/api/v1/namespaces/ns1/pods?&labelSelector=app%3Dapp2".to_string(), pod_list_ns1app2response),
            ("/api/v1/namespaces/ns2/pods?&labelSelector=app%3Dapp3".to_string(), pod_list_ns2app3response),
            ("/api/v1/namespaces/ns2/pods?&labelSelector=app%3Dapp4".to_string(), pod_list_ns2app4response),
            ("/api/v1/namespaces/ns1/pods/ns1app1pod1?&fieldManager=kdynaroll_patch_call".to_string(), pod_patch_response.clone()),
            ("/api/v1/namespaces/ns1/pods/ns1app1pod2?&fieldManager=kdynaroll_patch_call".to_string(), pod_patch_response.clone()),
            ("/api/v1/namespaces/ns1/pods/ns1app2pod3?&fieldManager=kdynaroll_patch_call".to_string(), pod_patch_response.clone()),
            ("/api/v1/namespaces/ns1/pods/ns1app2pod4?&fieldManager=kdynaroll_patch_call".to_string(), pod_patch_response.clone()),
            ("/api/v1/namespaces/ns2/pods/ns2app3pod5?&fieldManager=kdynaroll_patch_call".to_string(), pod_patch_response.clone()),
            ("/api/v1/namespaces/ns2/pods/ns2app3pod6?&fieldManager=kdynaroll_patch_call".to_string(), pod_patch_response.clone()),
            ("/api/v1/namespaces/ns2/pods/ns2app4pod7?&fieldManager=kdynaroll_patch_call".to_string(), pod_patch_response.clone()),
            ("/api/v1/namespaces/ns2/pods/ns2app4pod8?&fieldManager=kdynaroll_patch_call".to_string(), pod_patch_response.clone()),
        ]);

        let num_calls = response_content.len() as i32; // 2 namespaces, 2 apps per namespace, 3 pods per app

        return (response_content, num_calls);
    }

    fn make_full_reconcile_expected_response() -> HashMap<String, RequestContent> {
        HashMap::<String, RequestContent>::from([
            (
                "/api/v1/namespaces/ns1/pods/ns1app2pod4?&fieldManager=kdynaroll_patch_call".to_string(),
                RequestContent {
                    body: "{\"metadata\":{\"labels\":{\"label2\":\"label2app2value\"}}}".to_string(),
                    method: http::Method::PATCH,
                },
            ),
            (
                "/api/v1/namespaces/ns2/pods/ns2app4pod7?&fieldManager=kdynaroll_patch_call".to_string(),
                RequestContent {
                    body: "{\"metadata\":{\"labels\":{\"label4\":\"label4app4value\"}}}".to_string(),
                    method: http::Method::PATCH,
                },
            ),
            (
                "/api/v1/namespaces/ns1/pods?&labelSelector=app%3Dapp1".to_string(),
                RequestContent {
                    body: "".to_string(),
                    method: http::Method::GET,
                },
            ),
            (
                "/api/v1/namespaces/ns2/pods?&labelSelector=app%3Dapp3".to_string(),
                RequestContent {
                    body: "".to_string(),
                    method: http::Method::GET,
                },
            ),
            (
                "/api/v1/namespaces/ns1/pods/ns1app1pod1?&fieldManager=kdynaroll_patch_call".to_string(),
                RequestContent {
                    body: "{\"metadata\":{\"labels\":{\"label1app1\":\"label1app1value\"}}}".to_string(),
                    method: http::Method::PATCH,
                },
            ),
            (
                "/api/v1/namespaces/ns2/pods/ns2app3pod5?&fieldManager=kdynaroll_patch_call".to_string(),
                RequestContent {
                    body: "{\"metadata\":{\"labels\":{\"label3\":\"label3app3value\"}}}".to_string(),
                    method: http::Method::PATCH,
                },
            ),
            (
                "/api/v1/namespaces/ns1/pods/ns1app2pod4?&fieldManager=kdynaroll_patch_call".to_string(),
                RequestContent {
                    body: "{\"metadata\":{\"labels\":{\"label2\":\"label2app2value\"}}}".to_string(),
                    method: http::Method::PATCH,
                },
            ),
            (
                "/api/v1/namespaces/ns1/pods/ns1app2pod3?&fieldManager=kdynaroll_patch_call".to_string(),
                RequestContent {
                    body: "{\"metadata\":{\"labels\":{\"label2\":\"label2app2value\"}}}".to_string(),
                    method: http::Method::PATCH,
                },
            ),
            (
                "/api/v1/namespaces/ns1/pods?&labelSelector=app%3Dapp2".to_string(),
                RequestContent {
                    body: "".to_string(),
                    method: http::Method::GET,
                },
            ),
            (
                "/api/v1/namespaces/ns2/pods/ns2app3pod6?&fieldManager=kdynaroll_patch_call".to_string(),
                RequestContent {
                    body: "{\"metadata\":{\"labels\":{\"label3\":\"label3app3value\"}}}".to_string(),
                    method: http::Method::PATCH,
                },
            ),
            (
                "/api/v1/namespaces/ns1/pods/ns1app1pod2?&fieldManager=kdynaroll_patch_call".to_string(),
                RequestContent {
                    body: "{\"metadata\":{\"labels\":{\"label1app1\":\"label1app1value\"}}}".to_string(),
                    method: http::Method::PATCH,
                },
            ),
            (
                "/api/v1/namespaces/ns2/pods?&labelSelector=app%3Dapp4".to_string(),
                RequestContent {
                    body: "".to_string(),
                    method: http::Method::GET,
                },
            ),
            (
                "/api/v1/namespaces/ns2/pods/ns2app4pod8?&fieldManager=kdynaroll_patch_call".to_string(),
                RequestContent {
                    body: "{\"metadata\":{\"labels\":{\"label4\":\"label4app4value\"}}}".to_string(),
                    method: http::Method::PATCH,
                },
            ),
        ])
    }
}
