//! `declare-instrument` carries `metrology` and `calibration` to Core as given,
//! hashes a certificate file, refuses a bad block before Core is called, and a
//! re-declare keeps what is stored (decision 138). Against a mock Core.

mod support;

use serde_json::{json, Value};
use support::{Behavior, MockCore, RecordedRequest};

const STORED: &str = r#"{"name":"dmm","protocol":"scpi","port":{"vid":6790,"pid":29987},"baud":9600,
    "write_terminator":"\n","read_terminator":"\r\n","min_interval_ms":0,"safe_off":[],
    "metrology":[{"quantity":"voltage","unit":"V","resolution":0.001,"U":0.01,"k":2.0,"basis":"datasheet","range":{"min":0.0,"max":10.0}}],
    "calibration":{"serial":"S1","certificate_ref":"C-1","due":"2027-01-31"}}"#;

const CURRENT: &str = r#"{"quantity":"current","unit":"A","resolution":0.0001,"U":0.002,"k":2.0,"basis":"declared","range":{"min":0.0,"max":5.0}}"#;

async fn run(extra: &[&str]) -> (std::process::Output, Vec<RecordedRequest>) {
    let stored: Value = serde_json::from_str(STORED).unwrap();
    let mock = MockCore::start(Behavior::Router {
        routes: vec![("/instruments".to_string(), Behavior::Sequence(vec![Behavior::json_ok(json!([stored.clone()])), Behavior::json_ok(stored.clone()), Behavior::json_ok(json!([stored]))]))],
        otherwise: Box::new(Behavior::json_ok(json!({}))),
    })
    .await;
    let dir = support::unique_dir("metrology");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = dir.join("api.toml");
    std::fs::write(&cfg, format!("[core]\nbase_url = \"{}\"\ntoken = \"t\"\n", mock.base_url())).unwrap();
    std::fs::write(dir.join("cert.pdf"), b"abc").unwrap();
    let cert = dir.join("cert.pdf").display().to_string();
    let extra: Vec<String> = extra.iter().map(|a| a.replace("CERT", &cert)).collect();
    let out = support::api_command(env!("CARGO_BIN_EXE_embarch-api"))
        .arg("--config")
        .arg(&cfg)
        .args(["declare-instrument", "dmm", "--port", "1a86:7523", "--baud", "9600"])
        .args(&extra)
        .env("XDG_DATA_HOME", dir.join("data"))
        .env("LOCALAPPDATA", dir.join("data"))
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    (out, mock.requests())
}

fn put(reqs: &[RecordedRequest]) -> Option<Value> {
    reqs.iter().find(|r| r.method == "PUT").map(|r| serde_json::from_slice(&r.body).unwrap())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_blocks_are_sent_as_given_and_the_certificate_is_hashed() {
    let cal = r#"{"serial":"S2","certificate_ref":"C-2","due":"2027-02-28","certificate":"CERT"}"#;
    let (out, reqs) = run(&["--metrology", CURRENT, "--calibration", cal]).await;
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let body = put(&reqs).unwrap();
    assert_eq!(body["metrology"], json!([serde_json::from_str::<Value>(CURRENT).unwrap()]));
    assert_eq!(body["calibration"]["due"], "2027-02-28");
    assert_eq!(body["calibration"]["certificate_sha256"], "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    assert!(body["calibration"].get("certificate").is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bad_block_is_refused_and_core_is_not_called() {
    let bad_date = r#"{"serial":"S","certificate_ref":"C","due":"2027-02-30"}"#;
    let neg_u = CURRENT.replace("0.002", "-0.002");
    let neg_res = CURRENT.replace("0.0001", "-1.0");
    let inverted = CURRENT.replace(r#""min":0.0,"max":5.0"#, r#""min":9.0,"max":5.0"#);
    for args in [
        vec!["--calibration", bad_date],
        vec!["--metrology", neg_u.as_str()],
        vec!["--metrology", neg_res.as_str()],
        vec!["--metrology", inverted.as_str()],
    ] {
        let (out, reqs) = run(&args).await;
        assert!(!out.status.success(), "{args:?}");
        assert!(reqs.is_empty(), "Core was called for {args:?}: {reqs:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_redeclare_keeps_what_is_stored_and_empty_clears() {
    let (out, reqs) = run(&[]).await;
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let body = put(&reqs).unwrap();
    assert_eq!(body["metrology"][0]["quantity"], "voltage");
    assert_eq!(body["calibration"]["serial"], "S1");
    let (out, reqs) = run(&["--clear-metrology", "--clear-calibration"]).await;
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let body = put(&reqs).unwrap();
    assert!(body.get("metrology").is_none() && body.get("calibration").is_none(), "{body}");
}
