// 收藏：索引落在 favorites.json，封面缓存落在 covers\<id>.jpg
//
// 约定（按需求定的）：
// - 收藏时把封面下载到本地（1000×1000，和窗口用的档位一致），之后这张离线也能看；
// - 取消收藏时本地图片一并删掉（不留垃圾）；
// - 索引顺序 = 收藏顺序（新的追加在后面），轮播就按这个顺序走。
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

#[derive(Clone, Serialize, Deserialize)]
pub struct Favorite {
    pub id: String,
    pub sf: String,
    pub title: String,
    pub artist: String,
    #[serde(default)]
    pub date: String,
    #[serde(default)]
    pub genre: String,
    #[serde(default)]
    pub tracks: u32,
    /// 图床地址（不带尺寸段），本地缓存失败时用它回退到网络
    pub art: String,
    #[serde(default)]
    pub saved_at: u64,
    /// 本地封面文件名（covers/<id>.jpg）；下载失败时为空字符串
    #[serde(default)]
    pub cover: String,
}

#[derive(Serialize, Deserialize, Default)]
struct Index {
    items: Vec<Favorite>,
}

fn data_dir(app: &AppHandle) -> Option<PathBuf> {
    app.path().app_data_dir().ok()
}

fn index_path(app: &AppHandle) -> Option<PathBuf> {
    data_dir(app).map(|d| d.join("favorites.json"))
}

/// 本地封面目录（asset 协议的白名单也指向这里）
pub fn covers_dir(app: &AppHandle) -> Option<PathBuf> {
    data_dir(app).map(|d| d.join("covers"))
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn load(app: &AppHandle) -> Vec<Favorite> {
    let Some(path) = index_path(app) else { return Vec::new() };
    let Ok(text) = fs::read_to_string(path) else { return Vec::new() };
    serde_json::from_str::<Index>(&text).map(|i| i.items).unwrap_or_default()
}

fn save(app: &AppHandle, items: &[Favorite]) -> Result<(), String> {
    let Some(path) = index_path(app) else { return Err("找不到应用数据目录".into()) };
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string_pretty(&Index { items: items.to_vec() })
        .map_err(|e| e.to_string())?;
    // 临时文件 + rename，避免写一半断电留下坏文件
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, text).map_err(|e| e.to_string())?;
    fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

/// 下载封面到 covers/<id>.jpg（失败不阻塞收藏，只是这张要联网看）
async fn download_cover(item: &Favorite, dir: &PathBuf) -> Result<String, String> {
    let url = format!("{}/1000x1000bb.jpg", item.art);
    let res = reqwest::get(&url).await.map_err(|e| e.to_string())?;
    if !res.status().is_success() {
        return Err(format!("HTTP {}", res.status()));
    }
    let bytes = res.bytes().await.map_err(|e| e.to_string())?;
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let name = format!("{}.jpg", item.id);
    fs::write(dir.join(&name), &bytes).map_err(|e| e.to_string())?;
    Ok(name)
}

#[tauri::command]
pub fn list_favorites(app: AppHandle) -> Vec<Favorite> {
    load(&app)
}

/// 本地封面目录的绝对路径：界面用它拼 covers/<id>.jpg，再交给 convertFileSrc
#[tauri::command]
pub fn covers_dir_path(app: AppHandle) -> String {
    covers_dir(&app).map(|p| p.to_string_lossy().to_string()).unwrap_or_default()
}

#[tauri::command]
pub async fn add_favorite(app: AppHandle, album: Favorite) -> Result<Vec<Favorite>, String> {
    let mut items = load(&app);
    if items.iter().any(|f| f.id == album.id && f.sf == album.sf) {
        return Ok(items); // 已收藏：幂等，不重复、不改顺序
    }
    let mut item = album;
    item.saved_at = now();
    if let Some(dir) = covers_dir(&app) {
        if let Ok(name) = download_cover(&item, &dir).await {
            item.cover = name;
        }
    }
    items.push(item);
    save(&app, &items)?;
    Ok(items)
}

#[tauri::command]
pub fn remove_favorite(app: AppHandle, id: String, sf: String) -> Result<Vec<Favorite>, String> {
    let mut items = load(&app);
    if let Some(at) = items.iter().position(|f| f.id == id && f.sf == sf) {
        let item = items.remove(at);
        // 按约定：取消收藏就把本地缓存删掉，不留垃圾
        if let Some(dir) = covers_dir(&app) {
            let _ = fs::remove_file(dir.join(format!("{}.jpg", item.id)));
        }
    }
    save(&app, &items)?;
    Ok(items)
}
