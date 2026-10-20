//! Porkbun's JSON API (v3). Keys are created at porkbun.com/account/api, and API
//! access must be switched on for each domain in its settings.

use anyhow::{Context, bail};
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::Provider;

pub const API: &str = "https://api.porkbun.com/api/json/v3";

#[derive(Clone, Serialize, Deserialize)]
pub struct Keys {
    pub api_key: String,
    pub secret_key: String,
}

pub struct Porkbun {
    keys: Keys,
    base: String,
    client: reqwest::Client,
}

impl Porkbun {
    pub fn new(keys: Keys, base: &str) -> anyhow::Result<Self> {
        Ok(Self {
            keys,
            base: base.trim_end_matches('/').to_string(),
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()?,
        })
    }

    async fn call(&self, path: &str, mut body: Value) -> anyhow::Result<Value> {
        body["apikey"] = json!(self.keys.api_key);
        body["secretapikey"] = json!(self.keys.secret_key);
        let response = self
            .client
            .post(format!("{}/{path}", self.base))
            .json(&body)
            .send()
            .await
            .context("cannot reach Porkbun")?;
        let status = response.status();
        let value: Value = response.json().await.unwrap_or(Value::Null);
        if value["status"] != "SUCCESS" {
            let message = value["message"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| format!("HTTP {status}"));
            bail!("Porkbun: {message}");
        }
        Ok(value)
    }

    /// `type/zone/label` path suffix; the apex has no label.
    fn by_name(zone: &str, kind: &str, label: &str) -> String {
        if label.is_empty() {
            format!("{zone}/{kind}")
        } else {
            format!("{zone}/{kind}/{label}")
        }
    }
}

fn address_kind(ip: &str) -> &'static str {
    if ip.contains(':') { "AAAA" } else { "A" }
}

impl Provider for Porkbun {
    fn name(&self) -> &'static str {
        "porkbun"
    }

    fn zones(&self) -> BoxFuture<'_, anyhow::Result<Vec<String>>> {
        Box::pin(async move {
            let value = self.call("domain/listAll", json!({ "start": "0" })).await?;
            let mut zones: Vec<String> = value["domains"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|d| d["domain"].as_str().map(str::to_lowercase))
                .collect();
            zones.sort();
            Ok(zones)
        })
    }

    fn set_address<'a>(
        &'a self,
        zone: &'a str,
        label: &'a str,
        ip: &'a str,
    ) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            let kind = address_kind(ip);
            let existing = self
                .call(
                    &format!(
                        "dns/retrieveByNameType/{}",
                        Self::by_name(zone, kind, label)
                    ),
                    json!({}),
                )
                .await?;
            let present = existing["records"]
                .as_array()
                .is_some_and(|r| !r.is_empty());
            if present {
                self.call(
                    &format!("dns/editByNameType/{}", Self::by_name(zone, kind, label)),
                    json!({ "content": ip, "ttl": 600 }),
                )
                .await?;
            } else {
                self.call(
                    &format!("dns/create/{zone}"),
                    json!({ "name": label, "type": kind, "content": ip, "ttl": 600 }),
                )
                .await?;
            }
            Ok(())
        })
    }

    fn delete_address<'a>(
        &'a self,
        zone: &'a str,
        label: &'a str,
        ip: &'a str,
    ) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            let kind = address_kind(ip);
            self.call(
                &format!("dns/deleteByNameType/{}", Self::by_name(zone, kind, label)),
                json!({}),
            )
            .await?;
            Ok(())
        })
    }

    fn add_txt<'a>(
        &'a self,
        zone: &'a str,
        label: &'a str,
        value: &'a str,
    ) -> BoxFuture<'a, anyhow::Result<String>> {
        Box::pin(async move {
            let created = self
                .call(
                    &format!("dns/create/{zone}"),
                    json!({ "name": label, "type": "TXT", "content": value, "ttl": 600 }),
                )
                .await?;
            Ok(match &created["id"] {
                Value::String(id) => id.clone(),
                other => other.to_string(),
            })
        })
    }

    fn delete_record<'a>(
        &'a self,
        zone: &'a str,
        id: &'a str,
    ) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            self.call(&format!("dns/delete/{zone}/{id}"), json!({}))
                .await?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::Json;
    use axum::extract::{Path, State};
    use axum::routing::post;
    use parking_lot::Mutex;

    use super::*;

    type Log = Arc<Mutex<Vec<(String, Value)>>>;

    /// A fake Porkbun: one existing A record (`old.example.com`), wrong keys refused.
    async fn mock() -> (String, Log) {
        async fn handle(
            State(log): State<Log>,
            Path(path): Path<String>,
            Json(body): Json<Value>,
        ) -> Json<Value> {
            log.lock().push((path.clone(), body.clone()));
            if body["secretapikey"] != "sk1_good" {
                return Json(json!({ "status": "ERROR", "message": "Invalid API key." }));
            }
            Json(match path.as_str() {
                "domain/listAll" => json!({ "status": "SUCCESS", "domains": [
                    { "domain": "Example.com" }, { "domain": "other.dev" }
                ]}),
                "dns/retrieveByNameType/example.com/A/old" => {
                    json!({ "status": "SUCCESS", "records": [{ "id": "1", "content": "9.9.9.9" }] })
                }
                p if p.starts_with("dns/retrieveByNameType/") => {
                    json!({ "status": "SUCCESS", "records": [] })
                }
                "dns/create/example.com" => json!({ "status": "SUCCESS", "id": 4242 }),
                _ => json!({ "status": "SUCCESS" }),
            })
        }
        let log: Log = Default::default();
        let app = axum::Router::new()
            .route("/{*path}", post(handle))
            .with_state(log.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await });
        (format!("http://{addr}"), log)
    }

    fn client(base: &str, secret: &str) -> Porkbun {
        let keys = Keys {
            api_key: "pk1_x".into(),
            secret_key: secret.into(),
        };
        Porkbun::new(keys, base).unwrap()
    }

    #[tokio::test]
    async fn manages_records() {
        let (base, log) = mock().await;
        let porkbun = client(&base, "sk1_good");

        assert_eq!(porkbun.zones().await.unwrap(), ["example.com", "other.dev"]);
        porkbun
            .set_address("example.com", "app", "1.2.3.4")
            .await
            .unwrap();
        porkbun
            .set_address("example.com", "old", "1.2.3.4")
            .await
            .unwrap();
        porkbun
            .set_address("example.com", "", "2001:db8::1")
            .await
            .unwrap();
        porkbun
            .delete_address("example.com", "app", "1.2.3.4")
            .await
            .unwrap();
        let id = porkbun
            .add_txt("example.com", "_acme-challenge", "token")
            .await
            .unwrap();
        assert_eq!(id, "4242");
        porkbun.delete_record("example.com", &id).await.unwrap();

        let calls: Vec<String> = log.lock().iter().map(|(p, _)| p.clone()).collect();
        assert_eq!(
            calls,
            [
                "domain/listAll",
                "dns/retrieveByNameType/example.com/A/app",
                "dns/create/example.com",
                "dns/retrieveByNameType/example.com/A/old",
                "dns/editByNameType/example.com/A/old",
                "dns/retrieveByNameType/example.com/AAAA",
                "dns/create/example.com",
                "dns/deleteByNameType/example.com/A/app",
                "dns/create/example.com",
                "dns/delete/example.com/4242",
            ]
        );
        let bodies = log.lock();
        assert_eq!(bodies[2].1["name"], "app");
        assert_eq!(bodies[2].1["type"], "A");
        assert_eq!(bodies[2].1["content"], "1.2.3.4");
        assert_eq!(bodies[4].1["content"], "1.2.3.4");
        assert_eq!(bodies[6].1["type"], "AAAA");
        assert_eq!(bodies[8].1["type"], "TXT");
        assert_eq!(bodies[8].1["apikey"], "pk1_x");
    }

    #[tokio::test]
    async fn reports_errors() {
        let (base, _) = mock().await;
        let error = client(&base, "sk1_bad").zones().await.unwrap_err();
        assert_eq!(error.to_string(), "Porkbun: Invalid API key.");
    }
}
