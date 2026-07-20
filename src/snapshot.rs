use chaser_oxide::ChaserPage;
use rustc_hash::FxHashMap;

const SNAPSHOT_SCRIPT: &str = include_str!("snapshot.js");

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct SnapshotNode {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub element: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<[f32; 2]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<[f32; 2]>,
    #[serde(default, skip_serializing_if = "FxHashMap::is_empty")]
    pub attributes: FxHashMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<SnapshotNode>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
}

pub async fn snapshot(page: &ChaserPage) -> Result<SnapshotNode, crate::Error> {
    let snapshot = page
        .evaluate_stealth(SNAPSHOT_SCRIPT)
        .await?
        .expect("Snapshot returned no data");

    Ok(serde_json::from_value(snapshot)?)
}
