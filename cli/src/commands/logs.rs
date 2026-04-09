use anyhow::Result;
use futures::StreamExt;

use crate::client::NemoClient;

/// One-shot dump of the active pod's container logs (#99).
///
/// Hits the /pod-logs/{id} endpoint which reads kubernetes pod logs
/// directly, bypassing the Postgres log stream. Works mid-run and
/// gives an operator the same information `kubectl logs -c agent`
/// would, without requiring kubectl access.
pub async fn run_tail(
    client: &NemoClient,
    loop_id: &str,
    tail_lines: u32,
    container: &str,
) -> Result<()> {
    let path = format!("/pod-logs/{loop_id}?tail={tail_lines}&container={container}");
    let resp = client.get_stream(&path).await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        anyhow::bail!("pod-logs returned {status}: {text}");
    }
    print!("{text}");
    if !text.ends_with('\n') {
        println!();
    }
    Ok(())
}

pub async fn run(
    client: &NemoClient,
    loop_id: &str,
    round: Option<i32>,
    stage: Option<String>,
) -> Result<()> {
    let mut path = format!("/logs/{loop_id}");
    let mut params = vec![];

    if let Some(r) = round {
        params.push(format!("round={r}"));
    }
    if let Some(ref s) = stage {
        // URL-encode the stage value to handle reserved characters
        let encoded: String = s
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                    c.to_string()
                } else {
                    format!("%{:02X}", c as u32)
                }
            })
            .collect();
        params.push(format!("stage={encoded}"));
    }
    if !params.is_empty() {
        path = format!("{path}?{}", params.join("&"));
    }

    let resp = client.get_stream(&path).await?;

    // Check content type for SSE vs JSON
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if content_type.contains("text/event-stream") {
        // SSE streaming
        let mut stream = resp.bytes_stream();
        let mut buffer = String::new();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            // Process complete SSE events
            while let Some(pos) = buffer.find("\n\n") {
                let event = &buffer[..pos];
                for line in event.lines() {
                    let Some(data) = line.strip_prefix("data: ") else {
                        continue;
                    };
                    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(data) else {
                        continue;
                    };
                    if parsed.get("type").and_then(|t| t.as_str()) == Some("end") {
                        println!("\n--- Loop ended: {} ---", parsed["state"]);
                        return Ok(());
                    }
                    if let Some(line_text) = parsed.get("line").and_then(|l| l.as_str()) {
                        let stage_name =
                            parsed.get("stage").and_then(|s| s.as_str()).unwrap_or("?");
                        let r = parsed.get("round").and_then(|r| r.as_i64()).unwrap_or(0);
                        println!("[{stage_name}/r{r}] {line_text}");
                    }
                }
                buffer = buffer[pos + 2..].to_string();
            }
        }
    } else {
        // JSON response (historical logs)
        let body = resp.text().await?;
        let logs: Vec<serde_json::Value> = serde_json::from_str(&body)?;

        for log in &logs {
            if let (Some(stage_name), Some(r), Some(line_text)) = (
                log.get("stage").and_then(|s| s.as_str()),
                log.get("round").and_then(|r| r.as_i64()),
                log.get("line").and_then(|l| l.as_str()),
            ) {
                println!("[{stage_name}/r{r}] {line_text}");
            }
        }
    }

    Ok(())
}
