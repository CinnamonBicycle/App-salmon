use std::time::Duration;

use crate::common::{create_supabase_cluster, spawn_test_server, wait_for_ready_or_failed};

/// Builds an in-memory tar containing a single edge function at `functions/main/index.ts` — the
/// one path `SupabaseBackend`'s `--main-service` invocation actually serves (see
/// `backends::supabase`'s `FUNCTIONS_MAIN_SERVICE_PATH` doc comment) — plus a `migrations/`
/// directory to confirm the "whole project tree extracts, only `functions/` is consumed" scope
/// decision (`docs/DESIGN.md` §11) doesn't reject or choke on other real Supabase project
/// directories.
fn sample_project_tar() -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());

    let mut dir_header = tar::Header::new_gnu();
    dir_header.set_entry_type(tar::EntryType::Directory);
    dir_header.set_size(0);
    dir_header.set_mode(0o755);
    dir_header.set_cksum();
    builder
        .append_data(&mut dir_header.clone(), "functions/", std::io::empty())
        .expect("append functions/");
    builder
        .append_data(&mut dir_header.clone(), "functions/main/", std::io::empty())
        .expect("append functions/main/");
    builder
        .append_data(&mut dir_header, "migrations/", std::io::empty())
        .expect("append migrations/");

    let index_ts = br#"Deno.serve(() => new Response("hello from app salmon"));"#;
    let mut file_header = tar::Header::new_gnu();
    file_header.set_size(index_ts.len() as u64);
    file_header.set_mode(0o644);
    file_header.set_cksum();
    builder
        .append_data(
            &mut file_header,
            "functions/main/index.ts",
            index_ts.as_slice(),
        )
        .expect("append index.ts");

    builder.into_inner().expect("finish tar")
}

#[tokio::test]
async fn valid_request_eventually_becomes_ready() {
    let server = spawn_test_server().await;
    let (status, body) = create_supabase_cluster(&server, 60, sample_project_tar()).await;
    assert_eq!(
        status,
        reqwest::StatusCode::ACCEPTED,
        "unexpected response: {body}"
    );
    let id = body["id"].as_str().expect("id present");

    let info = wait_for_ready_or_failed(&server, id, Duration::from_mins(5)).await;
    assert_eq!(
        info["status"], "ready",
        "supabase cluster did not become ready: {info}"
    );
    assert_eq!(info["connection"]["kind"], "supabase");
    assert!(
        info["connection"]["api_url"]
            .as_str()
            .is_some_and(|url| url.starts_with("http://127.0.0.1:"))
    );
    assert!(
        info["connection"]["anon_key"]
            .as_str()
            .is_some_and(|key| !key.is_empty())
    );
    assert!(
        info["connection"]["service_role_key"]
            .as_str()
            .is_some_and(|key| !key.is_empty())
    );
    assert!(
        info["connection"]["postgres"]["port"]
            .as_u64()
            .is_some_and(|p| p > 0)
    );
}
