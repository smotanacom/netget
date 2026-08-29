//! HTTP protocol validator for E2E tests

use anyhow::{Context, Result};
use reqwest::{Client, Response, StatusCode};
use serde_json::Value;
use std::time::Duration;

/// HTTP protocol validator with assertion helpers
pub struct HttpValidator {
    client: Client,
    base_url: String,
}

impl HttpValidator {
    /// Create a new HTTP validator for the given port
    pub fn new(port: u16) -> Self {
        let client = Client::builder()
            .resolve("127.0.0.1", std::net::SocketAddr::from(([127, 0, 0, 1], 0)))
            .timeout(Duration::from_secs(10))
            .build()
            .expect("Failed to build HTTP client");

        Self {
            client,
            base_url: format!("http://127.0.0.1:{}", port),
        }
    }

    /// Create with custom client configuration
    pub fn with_client(port: u16, client: Client) -> Self {
        Self {
            client,
            base_url: format!("http://127.0.0.1:{}", port),
        }
    }

    /// Send a GET request
    pub async fn get(&self, path: &str) -> Result<Response> {
        let url = format!("{}{}", self.base_url, path);
        self.client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("Failed to GET {}", url))
    }

    /// Send a POST request with JSON body
    pub async fn post_json(&self, path: &str, json: &Value) -> Result<Response> {
        let url = format!("{}{}", self.base_url, path);
        self.client
            .post(&url)
            .json(json)
            .send()
            .await
            .with_context(|| format!("Failed to POST to {}", url))
    }

    /// Send a POST request with text body
    pub async fn post_text(&self, path: &str, body: &str) -> Result<Response> {
        let url = format!("{}{}", self.base_url, path);
        self.client
            .post(&url)
            .body(body.to_string())
            .send()
            .await
            .with_context(|| format!("Failed to POST to {}", url))
    }

    /// Assert response has expected status code
    pub async fn expect_status(&self, path: &str, expected: StatusCode) -> Result<()> {
        let response = self.get(path).await?;
        let actual = response.status();

        if actual != expected {
            anyhow::bail!("Expected status {} for {}, got {}", expected, path, actual);
        }

        Ok(())
    }

    /// Assert response body contains text
    pub async fn expect_contains(&self, path: &str, text: &str) -> Result<()> {
        let response = self.get(path).await?;
        let body = response.text().await?;

        if !body.contains(text) {
            anyhow::bail!(
                "Expected response from {} to contain '{}', got: {}",
                path,
                text,
                body
            );
        }

        Ok(())
    }

    /// Assert response is valid JSON and matches expected value
    pub async fn expect_json(&self, path: &str, expected: &Value) -> Result<()> {
        let response = self.get(path).await?;
        let actual: Value = response
            .json()
            .await
            .context("Response is not valid JSON")?;

        if actual != *expected {
            anyhow::bail!(
                "JSON mismatch for {}:\nExpected: {}\nActual: {}",
                path,
                serde_json::to_string_pretty(expected)?,
                serde_json::to_string_pretty(&actual)?
            );
        }

        Ok(())
    }

    /// Assert response JSON contains a field with expected value
    pub async fn expect_json_field(&self, path: &str, field: &str, expected: &Value) -> Result<()> {
        let response = self.get(path).await?;
        let json: Value = response
            .json()
            .await
            .context("Response is not valid JSON")?;

        let actual = json
            .get(field)
            .with_context(|| format!("Field '{}' not found in JSON response", field))?;

        if actual != expected {
            anyhow::bail!(
                "Field '{}' mismatch for {}:\nExpected: {}\nActual: {}",
                field,
                path,
                expected,
                actual
            );
        }

        Ok(())
    }

    /// Check if server is reachable
    pub async fn is_reachable(&self) -> bool {
        self.get("/").await.is_ok()
    }

    /// Wait for server to become reachable
    pub async fn wait_for_ready(&self, max_attempts: u32) -> Result<()> {
        for i in 0..max_attempts {
            if self.is_reachable().await {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(500)).await;

            if i % 5 == 0 && i > 0 {
                println!(
                    "Still waiting for HTTP server to be ready... (attempt {})",
                    i
                );
            }
        }

        anyhow::bail!("HTTP server not reachable after {} attempts", max_attempts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validator_creation() {
        let validator = HttpValidator::new(8080);
        assert!(validator.base_url.contains("8080"));
    }
}
