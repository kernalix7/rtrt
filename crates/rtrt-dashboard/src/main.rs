//! rtrt-dashboard — axum web UI for token savings, recall stats, project templates.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::Result;
use axum::{
    Json, Router,
    extract::Path as AxPath,
    http::StatusCode,
    response::Html,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("rtrt=info,tower_http=info")
        .init();

    let bind =
        std::env::var("RTRT_DASHBOARD_BIND").unwrap_or_else(|_| "127.0.0.1:3111".to_string());
    let app = Router::new()
        .route("/", get(index))
        .route("/healthz", get(healthz))
        .route("/api/stats", get(stats))
        .route("/api/templates", get(list_templates))
        .route("/api/templates/{name}", get(get_template))
        .route("/api/templates/scaffold", post(scaffold));

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!("rtrt-dashboard listening on http://{bind}");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn healthz() -> &'static str {
    "ok"
}

async fn stats() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "input_saved": 0,
        "output_saved": 0,
        "provider": null,
    }))
}

async fn list_templates() -> Json<Vec<TemplateSummary>> {
    Json(
        rtrt_templates::list_all()
            .into_iter()
            .map(TemplateSummary::from)
            .collect(),
    )
}

async fn get_template(
    AxPath(name): AxPath<String>,
) -> std::result::Result<Json<rtrt_templates::Template>, (StatusCode, String)> {
    rtrt_templates::find(&name)
        .map(Json)
        .ok_or((StatusCode::NOT_FOUND, format!("template not found: {name}")))
}

#[derive(Debug, Clone, Serialize)]
struct TemplateSummary {
    name: String,
    description: String,
    source: rtrt_templates::TemplateSource,
    variables: Vec<rtrt_templates::TemplateVariable>,
}

impl From<rtrt_templates::Template> for TemplateSummary {
    fn from(t: rtrt_templates::Template) -> Self {
        Self {
            name: t.name,
            description: t.description,
            source: t.source,
            variables: t.variables,
        }
    }
}

#[derive(Debug, Deserialize)]
struct ScaffoldRequest {
    template: String,
    target: PathBuf,
    #[serde(default)]
    variables: BTreeMap<String, String>,
    #[serde(default)]
    overwrite: bool,
}

#[derive(Debug, Serialize)]
struct ScaffoldResponse {
    files_written: usize,
    root: PathBuf,
    post_hooks: Vec<String>,
}

async fn scaffold(
    Json(req): Json<ScaffoldRequest>,
) -> std::result::Result<Json<ScaffoldResponse>, (StatusCode, String)> {
    let tmpl = rtrt_templates::find(&req.template).ok_or((
        StatusCode::NOT_FOUND,
        format!("template not found: {}", req.template),
    ))?;
    let plan = rtrt_templates::render::plan(&tmpl, &req.target, req.variables)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    rtrt_templates::render::write(&plan, req.overwrite)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(ScaffoldResponse {
        files_written: plan.files.len(),
        root: plan.root,
        post_hooks: plan.post_hooks,
    }))
}

const INDEX_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>RTRT Dashboard</title>
<style>
  body { font: 14px/1.45 system-ui, sans-serif; max-width: 920px; margin: 2rem auto; padding: 0 1rem; }
  h1 { margin-bottom: 0.25rem; }
  .sub { color: #666; margin-bottom: 1.5rem; }
  section { margin-bottom: 2rem; }
  table { width: 100%; border-collapse: collapse; }
  th, td { padding: 0.4rem 0.6rem; border-bottom: 1px solid #eee; text-align: left; }
  code { background: #f3f3f3; padding: 0 0.25rem; border-radius: 3px; }
  button { padding: 0.4rem 0.8rem; border: 1px solid #888; background: #fff; cursor: pointer; }
</style>
</head>
<body>
<h1>RTRT</h1>
<div class="sub">Rust-based Token Reduction Toolkit</div>

<section>
  <h2>Token savings</h2>
  <div id="stats">loading…</div>
</section>

<section>
  <h2>Project templates</h2>
  <table id="tpl"><thead><tr><th>Name</th><th>Source</th><th>Description</th></tr></thead><tbody></tbody></table>
</section>

<script>
async function load() {
  const stats = await fetch('/api/stats').then(r => r.json());
  document.getElementById('stats').textContent =
    `input saved: ${stats.input_saved}  ·  output saved: ${stats.output_saved}`;
  const tpls = await fetch('/api/templates').then(r => r.json());
  const tbody = document.querySelector('#tpl tbody');
  tbody.innerHTML = tpls.map(t => `
    <tr>
      <td><code>${t.name}</code></td>
      <td>${t.source}</td>
      <td>${t.description}</td>
    </tr>`).join('');
}
load();
</script>
</body>
</html>
"#;
