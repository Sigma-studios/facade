//! Pebble's `pebble-challtestsrv` as a DNS provider, for end-to-end ACME tests only
//! (`--features pebble-test`). Credentials: management URL and comma-separated zones.

use futures_util::future::BoxFuture;
use serde_json::json;

use super::Provider;

pub struct ChallTestSrv {
    pub url: String,
    pub zones: Vec<String>,
    pub client: reqwest::Client,
}

impl ChallTestSrv {
    async fn post(&self, path: &str, body: serde_json::Value) -> anyhow::Result<()> {
        self.client
            .post(format!("{}/{path}", self.url))
            .json(&body)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
}

fn fqdn(zone: &str, label: &str) -> String {
    if label.is_empty() {
        format!("{zone}.")
    } else {
        format!("{label}.{zone}.")
    }
}

impl Provider for ChallTestSrv {
    fn name(&self) -> &'static str {
        "challtestsrv"
    }
    fn zones(&self) -> BoxFuture<'_, anyhow::Result<Vec<String>>> {
        Box::pin(async move { Ok(self.zones.clone()) })
    }
    fn set_address<'a>(
        &'a self,
        zone: &'a str,
        label: &'a str,
        ip: &'a str,
    ) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(self.post(
            "add-a",
            json!({ "host": fqdn(zone, label), "addresses": [ip] }),
        ))
    }
    fn delete_address<'a>(
        &'a self,
        zone: &'a str,
        label: &'a str,
        _ip: &'a str,
    ) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(self.post("clear-a", json!({ "host": fqdn(zone, label) })))
    }
    fn add_txt<'a>(
        &'a self,
        zone: &'a str,
        label: &'a str,
        value: &'a str,
    ) -> BoxFuture<'a, anyhow::Result<String>> {
        Box::pin(async move {
            let host = fqdn(zone, label);
            self.post("set-txt", json!({ "host": host, "value": value }))
                .await?;
            Ok(host)
        })
    }
    fn delete_record<'a>(
        &'a self,
        _zone: &'a str,
        id: &'a str,
    ) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(self.post("clear-txt", json!({ "host": id })))
    }
}
