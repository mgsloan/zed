//! Benchmark for the typst preview pipeline (warm LSP + comemo path).
//!
//! This is an example rather than a `#[bench]`/criterion harness because it
//! drives a real external `tinymist` binary against a real `.typ` document on
//! disk — infrastructure that isn't available in CI, so it can't run as part of
//! `cargo bench`/`cargo test`.
//!
//! It drives tinymist LSP over stdin/stdout, opens the document, starts the
//! preview server, then sends `textDocument/didChange` edits over the LSP and
//! receives page images over the WebSocket data plane. This exercises the real
//! incremental compilation path with comemo memoization — the same path used
//! when the user types in the editor.
//!
//! It doubles as the **end-to-end check on the protocol**: it is the only thing
//! that negotiates the subprotocol against a real tinymist, parses real frames,
//! and decodes real payloads. If the wire format and the server disagree, this
//! is where it shows.
//!
//! Run with:
//!   cargo run --release --example typst_preview_bench
//!
//! Environment variables:
//!   TINYMIST_BIN      — path to tinymist binary (default: ~/src/semitenn/tinymist/target/release/tinymist)
//!   TYPST_BENCH_FILE  — path to .typ document (default: ~/Documents/Law/David/deepdives.typ)
//!   BENCH_ITERS       — number of edit-compile-rasterize iterations (default: 10)

// This standalone benchmark harness deliberately drives tinymist with blocking
// `std::process`/`std::io`. It runs outside GPUI on its own thread (or under
// `smol::block_on`), so blocking the current thread is intended — and much
// simpler than wiring up async child stdio for a throwaway benchmark.
#![allow(clippy::disallowed_methods)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use async_tungstenite::WebSocketStream;
use async_tungstenite::tungstenite::Message;
use async_tungstenite::tungstenite::client::IntoClientRequest as _;
use futures::StreamExt as _;

use smol::net::TcpStream;
use typst_viewer::decode::decode_page;
use typst_viewer::protocol::{SUBPROTOCOL, ServerMessage, parse_frame};

fn main() {
    let Some(bin) = tinymist_bin() else {
        eprintln!(
            "tinymist binary not found. Set TINYMIST_BIN, or ensure \
             ~/src/semitenn/tinymist/target/release/tinymist exists, or put \
             tinymist in PATH."
        );
        return;
    };
    let Some(doc_path) = find_test_document() else {
        eprintln!(
            "No test document found. Set TYPST_BENCH_FILE or place a .typ file \
             at ~/Documents/Law/David/deepdives.typ"
        );
        return;
    };

    bench_preview_lsp(&bin, &doc_path);
}

// -----------------------------------------------------------------------
// Configuration
// -----------------------------------------------------------------------

fn bench_iters() -> usize {
    std::env::var("BENCH_ITERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10usize)
}

fn tinymist_bin() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("TINYMIST_BIN") {
        return Some(PathBuf::from(p));
    }
    if let Some(home) = home_dir() {
        let candidate = home.join("src/semitenn/tinymist/target/release/tinymist");
        if candidate.exists() {
            return Some(candidate);
        }
    }
    for dir in std::env::var("PATH").unwrap_or_default().split(':') {
        let candidate = PathBuf::from(dir).join("tinymist");
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

fn find_test_document() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("TYPST_BENCH_FILE") {
        let path = PathBuf::from(p);
        if !path.exists() {
            eprintln!("TYPST_BENCH_FILE does not exist: {path:?}");
            return None;
        }
        return Some(path);
    }
    if let Some(home) = home_dir() {
        let candidates = [
            home.join("Documents/Law/David/deepdives.typ"),
            home.join("Documents/Law/FoL/Assignments/1.2 S26 NASA/newton-principia-acoustica.typ"),
        ];
        for c in &candidates {
            if c.exists() {
                return Some(c.clone());
            }
        }
    }
    None
}

fn home_dir() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(PathBuf::from)
}

// -----------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------

fn replace_line(content: &str, line_idx: usize, new_line: &str) -> String {
    content
        .lines()
        .enumerate()
        .map(|(i, l)| if i == line_idx { new_line } else { l })
        .collect::<Vec<_>>()
        .join("\n")
}

fn find_heading_line(content: &str) -> Option<(usize, String)> {
    for (i, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with('=') && trimmed.len() > 5 {
            return Some((i, line.to_string()));
        }
    }
    for (i, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.len() > 20
            && !trimmed.starts_with("//")
            && !trimmed.starts_with('#')
            && trimmed.chars().any(|c| c.is_alphabetic())
        {
            return Some((i, line.to_string()));
        }
    }
    None
}

fn copy_dir_shallow(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).expect("create dest dir");
    let entries = std::fs::read_dir(src).expect("read source dir");
    for entry in entries {
        let entry = entry.expect("dir entry");
        let file_type = entry.file_type().expect("file type");
        if file_type.is_file() {
            let dest = dst.join(entry.file_name());
            std::fs::copy(entry.path(), &dest).expect("copy file");
        }
    }
}

/// Connects to the data plane, negotiating the page-image subprotocol.
///
/// A refused upgrade means this tinymist predates the protocol.
async fn connect_page_images(port: u64) -> anyhow::Result<WebSocketStream<TcpStream>> {
    let addr = format!("127.0.0.1:{port}");
    let tcp = TcpStream::connect(&addr).await?;
    let mut request = format!("ws://{addr}").as_str().into_client_request()?;
    let headers = request.headers_mut();
    headers.insert("Origin", format!("http://{addr}").parse()?);
    headers.insert("Sec-WebSocket-Protocol", SUBPROTOCOL.parse()?);

    let (ws, response) = async_tungstenite::client_async(request, tcp)
        .await
        .map_err(|err| anyhow::anyhow!("handshake failed (is tinymist new enough?): {err}"))?;

    let agreed = response
        .headers()
        .get("Sec-WebSocket-Protocol")
        .and_then(|v| v.to_str().ok());
    anyhow::ensure!(
        agreed == Some(SUBPROTOCOL),
        "server did not agree to {SUBPROTOCOL}; got {agreed:?}"
    );
    Ok(ws)
}

/// One page image received and decoded.
struct ReceivedPage {
    payload_bytes: usize,
    decode: Duration,
    px: (u32, u32),
}

/// Reads frames until a page image arrives, decoding it the way the viewer does.
///
/// Table and error frames are reported rather than skipped silently: a run that
/// only ever sees `pages` means images are not flowing, which is exactly the
/// failure this harness exists to catch.
async fn receive_page_image(
    ws: &mut WebSocketStream<TcpStream>,
    label: &str,
) -> anyhow::Result<ReceivedPage> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if Instant::now() > deadline {
            anyhow::bail!("{label}: timeout waiting for a page image (30s)");
        }
        let frame = match ws.next().await {
            Some(Ok(Message::Binary(bytes))) => bytes.to_vec(),
            Some(Ok(Message::Text(text))) => text.as_bytes().to_vec(),
            Some(Ok(Message::Close(f))) => anyhow::bail!("{label}: WebSocket closed: {f:?}"),
            Some(Ok(_)) => continue,
            Some(Err(e)) => anyhow::bail!("{label}: WebSocket error: {e}"),
            None => anyhow::bail!("{label}: WebSocket stream ended"),
        };

        match parse_frame(&frame)? {
            Some(ServerMessage::Pages { total, full, .. }) => {
                eprintln!("  [pages] total={total} full={full}");
            }
            Some(ServerMessage::Error { content, msg }) => {
                anyhow::bail!("{label}: server refused to render {content}: {msg}");
            }
            Some(ServerMessage::Image {
                px_width,
                px_height,
                scale,
                encoding,
                payload,
                ..
            }) => {
                let bytes = &frame[payload.clone()];
                let start = Instant::now();
                decode_page(bytes, px_width, px_height, encoding, scale)?;
                return Ok(ReceivedPage {
                    payload_bytes: payload.len(),
                    decode: start.elapsed(),
                    px: (px_width, px_height),
                });
            }
            None => {}
        }
    }
}

// -----------------------------------------------------------------------
// Result + summary
// -----------------------------------------------------------------------

struct IterResult {
    compile_ms: f64,
    payload_bytes: usize,
    decode_ms: f64,
}

fn print_summary(results: &[IterResult]) {
    eprintln!();
    eprintln!("=== Summary ({} iterations) ===", results.len());
    if results.is_empty() {
        return;
    }
    // avg / p50 / p95 / min for a metric extracted from each iteration.
    let stats = |f: fn(&IterResult) -> f64| {
        let mut v: Vec<f64> = results.iter().map(f).collect();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let avg = v.iter().sum::<f64>() / v.len() as f64;
        let p95 = v[((v.len() as f64 * 0.95) as usize).min(v.len() - 1)];
        (avg, v[v.len() / 2], p95, v[0])
    };
    let metrics: [(&str, fn(&IterResult) -> f64); 3] = [
        ("compile", |r| r.compile_ms),
        ("decode", |r| r.decode_ms),
        ("total", |r| r.compile_ms + r.decode_ms),
    ];
    eprintln!(
        "              {:>8} {:>8} {:>8} {:>8}",
        "avg", "p50", "p95", "min"
    );
    for (name, f) in metrics {
        let (avg, p50, p95, min) = stats(f);
        eprintln!("{name:<12}: {avg:8.1} {p50:8.1} {p95:8.1} {min:8.1} ms");
    }
    let avg_kib =
        results.iter().map(|r| r.payload_bytes as f64).sum::<f64>() / results.len() as f64 / 1024.0;
    eprintln!("avg payload: {avg_kib:.1} KiB");
}

// -----------------------------------------------------------------------
// Shared edit loop logic
// -----------------------------------------------------------------------

fn chop_heading(current: &mut String, original: &str) {
    let trimmed = current.trim_end();
    if trimmed.len() <= 3 {
        *current = original.to_string();
    } else {
        let mut chars: Vec<char> = trimmed.chars().collect();
        chars.pop();
        *current = chars.into_iter().collect();
    }
}

fn bench_preview_lsp(bin: &Path, doc_path: &Path) {
    let iterations = bench_iters();

    eprintln!("=== Typst Preview Benchmark (LSP + comemo) ===");
    eprintln!("tinymist:   {}", bin.display());
    eprintln!("document:   {}", doc_path.display());
    eprintln!("iterations: {iterations}");
    eprintln!();

    let tmp_dir = std::env::temp_dir().join(format!("typst_bench_lsp_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp_dir);
    let src_dir = doc_path.parent().expect("document has no parent dir");
    copy_dir_shallow(src_dir, &tmp_dir);
    let work_doc = tmp_dir.join(doc_path.file_name().unwrap());
    assert!(work_doc.exists(), "working copy not found: {work_doc:?}");

    let original_content = std::fs::read_to_string(&work_doc).expect("read document");
    let (heading_line_idx, heading_line) =
        find_heading_line(&original_content).expect("document has no heading to mutate");
    eprintln!("mutating line {heading_line_idx}: {heading_line}");

    let root_uri = format!("file://{}", tmp_dir.display());
    let doc_uri = format!("file://{}", work_doc.display());

    smol::block_on(async {
        let mut lsp = LspProcess::start(bin, &tmp_dir);

        // Initialize LSP.
        let init_resp = lsp.request(
            1,
            "initialize",
            serde_json::json!({
                "processId": std::process::id(),
                "rootUri": root_uri,
                "capabilities": {
                    "textDocument": {
                        "synchronization": {
                            "didSave": true,
                            "dynamicRegistration": false
                        }
                    }
                },
                "initializationOptions": {
                    "formatterMode": "disable"
                }
            }),
        );
        assert!(
            init_resp.get("result").is_some(),
            "LSP init failed: {init_resp:?}"
        );
        lsp.notify("initialized", serde_json::json!({}));

        // Open the document.
        lsp.notify(
            "textDocument/didOpen",
            serde_json::json!({
                "textDocument": {
                    "uri": doc_uri,
                    "languageId": "typst",
                    "version": 1,
                    "text": original_content,
                }
            }),
        );

        // Give tinymist a moment to process didOpen before starting the
        // preview.  This example runs on `smol::block_on` outside GPUI, so the
        // GPUI executor timer isn't available here.
        smol::Timer::after(Duration::from_millis(500)).await;

        // Start preview.
        let preview_resp = lsp.request(
            2,
            "workspace/executeCommand",
            serde_json::json!({
                "command": "tinymist.doStartPreview",
                // No flag selects the mode: the subprotocol offered at connect
                // does. `--page-images` only gates availability server-side.
                "arguments": [[
                    "--page-images=true",
                    "--data-plane-host=127.0.0.1:0",
                    work_doc.to_str().unwrap()
                ]]
            }),
        );
        let preview_result = preview_resp.get("result").unwrap_or_else(|| {
            panic!("doStartPreview returned no result; full response: {preview_resp}")
        });
        let data_plane_port = preview_result
            .get("dataPlanePort")
            .and_then(|v| v.as_u64())
            .expect("no dataPlanePort in response");
        eprintln!("preview data plane port: {data_plane_port}");

        // Connect WebSocket.
        let mut ws = connect_page_images(data_plane_port)
            .await
            .expect("negotiating the page-image subprotocol");

        // No `current`: the server sends the page table on connect. Subscribing
        // is what starts images flowing.
        let view = serde_json::json!({
            "visible": [0],
            "prefetch": [1],
            "cached": [],
            "scale": 2.0,
            "encoding": "raw",
            "opaque": true,
        });
        ws.send(Message::text(format!("view\n{view}")))
            .await
            .expect("send view");

        let warmup = receive_page_image(&mut ws, "warmup")
            .await
            .expect("receive the first page image");
        eprintln!(
            "warmup: {}x{} px, {} KiB payload, decode {:.1}ms",
            warmup.px.0,
            warmup.px.1,
            warmup.payload_bytes / 1024,
            warmup.decode.as_secs_f64() * 1000.0,
        );
        eprintln!();

        // --- Benchmark loop ---
        let mut results: Vec<IterResult> = Vec::with_capacity(iterations);
        let mut current_heading = heading_line.clone();

        for i in 0..iterations {
            chop_heading(&mut current_heading, &heading_line);
            let new_content = replace_line(&original_content, heading_line_idx, &current_heading);

            // Document versions start at 2 (version 1 was the initial didOpen).
            let version = i as i64 + 2;
            let change_start = Instant::now();
            lsp.notify(
                "textDocument/didChange",
                serde_json::json!({
                    "textDocument": {
                        "uri": doc_uri,
                        "version": version,
                    },
                    "contentChanges": [{
                        "text": new_content,
                    }]
                }),
            );

            let page = receive_page_image(&mut ws, &format!("iter {i}"))
                .await
                .unwrap_or_else(|e| panic!("iter {i}: {e}"));
            let compile_dur = change_start.elapsed();

            let compile_ms = compile_dur.as_secs_f64() * 1000.0;
            let decode_ms = page.decode.as_secs_f64() * 1000.0;
            eprintln!(
                "iter {i:2}: compile={compile_ms:6.1}ms  payload={:7}B  decode={decode_ms:6.1}ms",
                page.payload_bytes,
            );
            results.push(IterResult {
                compile_ms,
                payload_bytes: page.payload_bytes,
                decode_ms,
            });
        }

        // Shutdown.
        let _ = lsp.request(99, "shutdown", serde_json::json!(null));
        lsp.notify("exit", serde_json::json!(null));
        drop(lsp);

        print_summary(&results);
    });

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

// ===================================================================
// Minimal LSP client over stdin/stdout
// ===================================================================

struct LspProcess {
    child: std::process::Child,
    stdin: std::io::BufWriter<std::process::ChildStdin>,
    stdout: std::io::BufReader<std::process::ChildStdout>,
}

impl LspProcess {
    fn start(bin: &Path, cwd: &Path) -> Self {
        let mut child = std::process::Command::new(bin)
            .arg("lsp")
            .current_dir(cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("failed to start tinymist lsp");

        let stdin = std::io::BufWriter::new(child.stdin.take().expect("no stdin"));
        let stdout = std::io::BufReader::new(child.stdout.take().expect("no stdout"));

        let stderr = child.stderr.take().expect("no stderr");
        std::thread::spawn(move || {
            use std::io::BufRead;
            let reader = std::io::BufReader::new(stderr);
            for line in reader.lines().map_while(Result::ok) {
                if line.contains("ERROR") || line.contains("WARN") {
                    eprintln!("  lsp stderr: {line}");
                }
            }
        });

        Self {
            child,
            stdin,
            stdout,
        }
    }

    fn send_raw(&mut self, msg: &serde_json::Value) {
        let body = serde_json::to_string(msg).expect("serialize JSON-RPC");
        let header = format!("Content-Length: {}\r\n\r\n", body.len());
        self.stdin
            .write_all(header.as_bytes())
            .expect("write header");
        self.stdin.write_all(body.as_bytes()).expect("write body");
        self.stdin.flush().expect("flush stdin");
    }

    fn read_msg(&mut self) -> serde_json::Value {
        use std::io::BufRead;
        let mut content_length: Option<usize> = None;
        loop {
            let mut line = String::new();
            self.stdout.read_line(&mut line).expect("read header line");
            let trimmed = line.trim();
            if trimmed.is_empty() {
                break;
            }
            if let Some(val) = trimmed.strip_prefix("Content-Length: ") {
                content_length = Some(val.trim().parse().expect("parse Content-Length"));
            }
        }
        let length = content_length.expect("no Content-Length header");
        let mut body = vec![0u8; length];
        std::io::Read::read_exact(&mut self.stdout, &mut body).expect("read body");
        serde_json::from_slice(&body).expect("parse JSON-RPC response")
    }

    fn request(&mut self, id: i64, method: &str, params: serde_json::Value) -> serde_json::Value {
        self.send_raw(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }));
        loop {
            let msg = self.read_msg();
            if msg.get("id").and_then(|v| v.as_i64()) == Some(id) {
                return msg;
            }
        }
    }

    fn notify(&mut self, method: &str, params: serde_json::Value) {
        self.send_raw(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }));
    }
}

impl Drop for LspProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}
