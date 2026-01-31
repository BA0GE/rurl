use std::process::Command;
use std::fs;
use std::io::Read;
use serde_json::Value;
use std::collections::HashMap;

// Server imports
use tokio::runtime::Runtime;
use tokio::net::TcpListener;
use axum::{
    extract::Query,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Redirect, Response},
    routing::{get, post, put},
    Json, Router,
};

const QQ_URL: &str = "https://dldir1.qq.com/qqfile/qq/PCQQ9.7.17/QQ9.7.17.29225.exe";
const QQ_MD5: &str = "8d05d41cc25219d2a8c118d20ebda5fd";
const RURL_BIN: &str = "./target/debug/rurl";
const OUTPUT_DIR: &str = "./tests/output";

fn calculate_md5(path: &str) -> String {
    let mut file = fs::File::open(path).expect("无法打开文件");
    let mut context = md5::Context::new();
    let mut buffer = [0; 65536];
    loop {
        let n = file.read(&mut buffer).expect("读取文件失败");
        if n == 0 { break; }
        context.consume(&buffer[..n]);
    }
    #[allow(deprecated)]
    let digest = context.compute();
    format!("{:x}", digest)
}

fn ensure_built() {
    let status = Command::new("cargo")
        .arg("build")
        .status()
        .expect("无法运行 cargo build");
    assert!(status.success(), "Cargo build 失败");
}

fn setup() {
    let _ = fs::create_dir_all(OUTPUT_DIR);
    ensure_built();
}

fn read_json(path: &str) -> Value {
    let mut file = fs::File::open(path).unwrap_or_else(|e| panic!("无法打开 JSON 文件 {}: {}", path, e));
    let mut content = String::new();
    file.read_to_string(&mut content).expect("读取 JSON 文件失败");
    serde_json::from_str(&content).expect("解析 JSON 失败")
}

// Mock Server
struct TestServer {
    _rt: Runtime,
    addr: String,
}

impl TestServer {
    fn new() -> Self {
        let rt = Runtime::new().unwrap();
        let addr = rt.block_on(async {
            let app = Router::new()
                .route("/user-agent", get(handle_user_agent))
                .route("/headers", get(handle_headers))
                .route("/post", post(handle_post))
                .route("/put", put(handle_put))
                .route("/basic-auth/:user/:pass", get(handle_basic_auth))
                .route("/redirect-to", get(handle_redirect))
                .route("/get", get(handle_get).head(handle_get));

            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            format!("http://{}", addr)
        });
        Self { _rt: rt, addr }
    }
}

async fn handle_user_agent(headers: HeaderMap) -> Json<Value> {
    let ua = headers.get("user-agent").and_then(|v| v.to_str().ok()).unwrap_or("");
    Json(serde_json::json!({ "user-agent": ua }))
}

async fn handle_headers(headers: HeaderMap) -> Json<Value> {
    let mut h = serde_json::Map::new();
    for (k, v) in &headers {
        let key_str = k.to_string();
        if key_str.eq_ignore_ascii_case("range") || key_str.eq_ignore_ascii_case("referer") { continue; }
        h.insert(key_str, Value::String(v.to_str().unwrap_or("").to_string()));
    }
    Json(serde_json::json!({ "headers": h }))
}

async fn handle_post(headers: HeaderMap, body: String) -> Json<Value> {
    let json_body: Option<Value> = serde_json::from_str(&body).ok();
    let mut form = serde_json::Map::new();
    if json_body.is_none() {
        for pair in body.split('&') {
            if let Some((k, v)) = pair.split_once('=') {
                let decoded_v = urlencoding::decode(v).unwrap_or(std::borrow::Cow::Borrowed(v));
                form.insert(k.to_string(), Value::String(decoded_v.to_string()));
            }
        }
    }
    
    let mut h_map = serde_json::Map::new();
    for (k, v) in &headers {
         let key_str = k.to_string();
         if key_str.eq_ignore_ascii_case("range") || key_str.eq_ignore_ascii_case("referer") { continue; }
         h_map.insert(key_str, Value::String(v.to_str().unwrap_or("").to_string()));
    }

    Json(serde_json::json!({
        "form": form,
        "json": json_body,
        "headers": h_map
    }))
}

async fn handle_put(headers: HeaderMap, body: String) -> Json<Value> {
    handle_post(headers, body).await
}

async fn handle_basic_auth(headers: HeaderMap) -> Json<Value> {
    let auth_header = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("");
    // user:password -> dXNlcjpwYXNzd29yZA==
    let is_correct = auth_header == "Basic dXNlcjpwYXNzd29yZA==";
    
    Json(serde_json::json!({
        "authenticated": is_correct,
        "user": if is_correct { "user" } else { "" }
    }))
}

async fn handle_redirect(Query(params): Query<HashMap<String, String>>) -> Response {
    if let Some(url) = params.get("url") {
        Redirect::to(url).into_response()
    } else {
        StatusCode::BAD_REQUEST.into_response()
    }
}

async fn handle_get(headers: HeaderMap) -> Json<Value> {
    let mut h = serde_json::Map::new();
    for (k, v) in &headers {
        let key_str = k.to_string();
        if key_str.eq_ignore_ascii_case("range") || key_str.eq_ignore_ascii_case("referer") { continue; }
        h.insert(key_str, Value::String(v.to_str().unwrap_or("").to_string()));
    }
    Json(serde_json::json!({
        "headers": h,
        "url": "http://localhost/get" 
    }))
}

#[test]
fn test_01_curl_download() {
    setup();
    let output_path = format!("{}/qq_curl.exe", OUTPUT_DIR);
    let _ = fs::remove_file(&output_path);

    println!("Running curl download...");
    let status = Command::new("curl")
        .arg("-L")
        .arg("-o")
        .arg(&output_path)
        .arg(QQ_URL)
        .status()
        .expect("无法运行 curl");
    
    assert!(status.success(), "Curl 下载失败");
    assert_eq!(calculate_md5(&output_path), QQ_MD5, "Curl 下载文件 MD5 不匹配");
}

#[test]
fn test_02_rurl_default() {
    setup();
    let output_path = format!("{}/qq_rurl_default.exe", OUTPUT_DIR);
    let _ = fs::remove_file(&output_path);
    let _ = fs::remove_file(format!("{}.meta", output_path));

    println!("Running rurl default download...");
    let status = Command::new(RURL_BIN)
        .arg("-o")
        .arg(&output_path)
        .arg(QQ_URL)
        .status()
        .expect("无法运行 rurl");
        
    assert!(status.success(), "Rurl 默认下载失败");
    assert_eq!(calculate_md5(&output_path), QQ_MD5, "Rurl 默认下载文件 MD5 不匹配");
}

#[test]
fn test_03_rurl_multithread() {
    setup();
    let output_path = format!("{}/qq_rurl_mt.exe", OUTPUT_DIR);
    let _ = fs::remove_file(&output_path);
    let _ = fs::remove_file(format!("{}.meta", output_path));

    println!("Running rurl multithread download...");
    let status = Command::new(RURL_BIN)
        .arg("-t")
        .arg("16")
        .arg("-o")
        .arg(&output_path)
        .arg(QQ_URL)
        .status()
        .expect("无法运行 rurl");
        
    assert!(status.success(), "Rurl 多线程下载失败");
    assert_eq!(calculate_md5(&output_path), QQ_MD5, "Rurl 多线程下载文件 MD5 不匹配");
}

#[test]
fn test_04_rurl_dynamic_split() {
    setup();
    let output_path = format!("{}/qq_rurl_dynamic.exe", OUTPUT_DIR);
    let _ = fs::remove_file(&output_path);
    let _ = fs::remove_file(format!("{}.meta", output_path));

    println!("Running rurl dynamic split download...");
    let status = Command::new(RURL_BIN)
        .arg("--dynamic-split")
        .arg("-o")
        .arg(&output_path)
        .arg(QQ_URL)
        .status()
        .expect("无法运行 rurl");
        
    assert!(status.success(), "Rurl 动态分块下载失败");
    assert_eq!(calculate_md5(&output_path), QQ_MD5, "Rurl 动态分块下载文件 MD5 不匹配");
}

#[test]
fn test_05_user_agent() {
    setup();
    let server = TestServer::new();
    let output_path = format!("{}/ua.json", OUTPUT_DIR);
    let _ = fs::remove_file(&output_path);
    
    let custom_ua = "RurlTestAgent/1.0";

    println!("Running rurl User-Agent test...");
    let status = Command::new(RURL_BIN)
        .arg("-A")
        .arg(custom_ua)
        .arg("-o")
        .arg(&output_path)
        .arg(format!("{}/user-agent", server.addr))
        .status()
        .expect("无法运行 rurl");

    assert!(status.success(), "Rurl UA 测试失败");
    let json = read_json(&output_path);
    assert_eq!(json["user-agent"], custom_ua, "User-Agent 不匹配");
}

#[test]
fn test_06_custom_headers() {
    setup();
    let server = TestServer::new();
    let output_path = format!("{}/headers.json", OUTPUT_DIR);
    let _ = fs::remove_file(&output_path);
    
    println!("Running rurl custom headers test...");
    let status = Command::new(RURL_BIN)
        .arg("-H")
        .arg("X-Test-Header: MyValue")
        .arg("-H")
        .arg("X-Another: 123")
        .arg("-o")
        .arg(&output_path)
        .arg(format!("{}/headers", server.addr))
        .status()
        .expect("无法运行 rurl");

    assert!(status.success(), "Rurl Header 测试失败");
    let json = read_json(&output_path);
    assert_eq!(json["headers"]["x-test-header"], "MyValue", "X-Test-Header 不匹配");
    assert_eq!(json["headers"]["x-another"], "123", "X-Another 不匹配");
}

#[test]
fn test_07_post_data() {
    setup();
    let server = TestServer::new();
    let output_path = format!("{}/post.json", OUTPUT_DIR);
    let _ = fs::remove_file(&output_path);
    
    println!("Running rurl POST data test...");
    let status = Command::new(RURL_BIN)
        .arg("-d")
        .arg("field1=value1&field2=value2")
        .arg("-o")
        .arg(&output_path)
        .arg(format!("{}/post", server.addr))
        .status()
        .expect("无法运行 rurl");

    assert!(status.success(), "Rurl POST 测试失败");
    let json = read_json(&output_path);
    assert_eq!(json["form"]["field1"], "value1", "POST field1 不匹配");
    assert_eq!(json["form"]["field2"], "value2", "POST field2 不匹配");
}

#[test]
fn test_08_json_data() {
    setup();
    let server = TestServer::new();
    let output_path = format!("{}/json.json", OUTPUT_DIR);
    let _ = fs::remove_file(&output_path);
    
    println!("Running rurl JSON data test...");
    let status = Command::new(RURL_BIN)
        .arg("--json")
        .arg(r#"{"key": "value", "num": 123}"#)
        .arg("-o")
        .arg(&output_path)
        .arg(format!("{}/post", server.addr))
        .status()
        .expect("无法运行 rurl");

    assert!(status.success(), "Rurl JSON 测试失败");
    let json = read_json(&output_path);
    assert_eq!(json["json"]["key"], "value", "JSON key 不匹配");
    assert_eq!(json["json"]["num"], 123, "JSON num 不匹配");
    assert_eq!(json["headers"]["content-type"], "application/json", "Content-Type 不匹配");
}

#[test]
fn test_09_custom_method() {
    setup();
    let server = TestServer::new();
    let output_path = format!("{}/put.json", OUTPUT_DIR);
    let _ = fs::remove_file(&output_path);
    
    println!("Running rurl custom method (PUT) test...");
    let status = Command::new(RURL_BIN)
        .arg("-X")
        .arg("PUT")
        .arg("-d")
        .arg("data=put")
        .arg("-o")
        .arg(&output_path)
        .arg(format!("{}/put", server.addr))
        .status()
        .expect("无法运行 rurl");

    assert!(status.success(), "Rurl PUT 测试失败");
    let json = read_json(&output_path);
    assert_eq!(json["form"]["data"], "put", "PUT data 不匹配");
}

#[test]
fn test_10_auth() {
    setup();
    let server = TestServer::new();
    let output_path = format!("{}/auth.json", OUTPUT_DIR);
    let _ = fs::remove_file(&output_path);
    
    println!("Running rurl Basic Auth test...");
    let status = Command::new(RURL_BIN)
        .arg("-u")
        .arg("user:password")
        .arg("-o")
        .arg(&output_path)
        .arg(format!("{}/basic-auth/user/password", server.addr))
        .status()
        .expect("无法运行 rurl");

    assert!(status.success(), "Rurl Auth 测试失败");
    let json = read_json(&output_path);
    assert_eq!(json["authenticated"], true, "Auth 状态不匹配");
    assert_eq!(json["user"], "user", "Auth 用户名不匹配");
}

#[test]
fn test_11_redirects() {
    setup();
    let server = TestServer::new();
    let output_path = format!("{}/redirect.json", OUTPUT_DIR);
    let _ = fs::remove_file(&output_path);
    
    let target_url = format!("{}/get", server.addr);
    let redirect_url = format!("{}/redirect-to?url={}", server.addr, urlencoding::encode(&target_url));

    println!("Running rurl Redirect (-L) test...");
    let status = Command::new(RURL_BIN)
        .arg("-L")
        .arg("-o")
        .arg(&output_path)
        .arg(redirect_url)
        .status()
        .expect("无法运行 rurl");

    assert!(status.success(), "Rurl Redirect 测试失败");
    let json = read_json(&output_path);
    // Should contain data from /get
    assert!(json.get("url").is_some(), "重定向未能获取目标内容");
    assert!(json["url"].as_str().unwrap().contains("/get"), "重定向目标 URL 不匹配");
}

#[test]
fn test_12_insecure() {
    setup();
    let output_path = format!("{}/insecure.html", OUTPUT_DIR);
    let _ = fs::remove_file(&output_path);
    
    // We use badssl.com for this test as it's hard to mock self-signed certs easily
    println!("Running rurl Insecure (-k) test...");
    let status = Command::new(RURL_BIN)
        .arg("-k")
        .arg("-o")
        .arg(&output_path)
        .arg("https://self-signed.badssl.com/")
        .status()
        .expect("无法运行 rurl");

    assert!(status.success(), "Rurl Insecure 测试失败");
    let content = fs::read_to_string(&output_path).expect("读取 insecure 输出失败");
    assert!(content.contains("badssl.com"), "内容不包含 badssl.com");
}

#[test]
fn test_13_head() {
    setup();
    let server = TestServer::new();
    
    println!("Running rurl HEAD (-I) test...");
    let output = Command::new(RURL_BIN)
        .arg("-I")
        .arg(format!("{}/get", server.addr))
        .output()
        .expect("无法运行 rurl");

    assert!(output.status.success(), "Rurl HEAD 测试失败");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("HTTP/1.1 200 OK"), "HEAD 输出不包含状态码");
    assert!(stdout.contains("content-type:"), "HEAD 输出不包含 header");
}

#[test]
fn test_14_include() {
    setup();
    let server = TestServer::new();
    
    println!("Running rurl Include (-i) test...");
    let output = Command::new(RURL_BIN)
        .arg("-i")
        .arg("-o")
        .arg("-") // Explicit stdout
        .arg(format!("{}/get", server.addr))
        .output()
        .expect("无法运行 rurl");

    assert!(output.status.success(), "Rurl Include 测试失败");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("HTTP/1.1 200 OK"), "Include 输出不包含状态码");
    assert!(stdout.contains("content-type:"), "Include 输出不包含 header");
    assert!(stdout.contains("\"url\":"), "Include 输出不包含 body");
}
