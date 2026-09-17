// 每日一屏：不再"每天现拉"，改成从候选池（pool.rs）里挑 20 张。
//
// 第一版参数（用户定的）：
// - 结构：现代 8 / 经典 7 / 混合 5
// - 经典那 7 张按年代分配：至少跨 3 个年代，单个年代最多 3 张
// - 混合里留 2 张"探索位"，只从 B/C 档里抽
// - 硬约束：专辑 60 天不重复、封面指纹 60 天不重复、同一歌手 1 张/天 + 7 天冷却
// - 流派上下限；最近 30 天占比高的流派/年代会被降权（长期均衡）
// - 池子还没建起来（< 40 张）时，退回"现抓一批"的老路径，保证第一屏永远有内容
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

use crate::pool::{self, art_key, norm_base, year_of, PoolAlbum, Rng};

/// 每天一屏 20 张
const PER_DAY: usize = 20;
/// 历史保留天数
const KEEP_DAYS: usize = 60;
/// 结构：现代 / 经典 / 混合
const N_MODERN: usize = 8;
const N_CLASSIC: usize = 7;
/// 混合里的探索位
const N_EXPLORE: usize = 2;
/// 同一歌手展示后多少天不再出现
const ARTIST_COOLDOWN_DAYS: i64 = 7;
/// 近几年算"现代"
const MODERN_YEARS: u16 = 6;
/// 池子小于这个数就先按老办法现抓
const POOL_MIN: usize = 40;

#[derive(Clone, Serialize, Deserialize)]
pub struct Album {
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
    /// 图床基址（不带尺寸段）
    pub art: String,
    /// 今天正好是这张的发行纪念日
    #[serde(default)]
    pub anniv: bool,
}

#[derive(Serialize, Deserialize, Clone)]
struct Day {
    date: String,
    #[serde(default)]
    albums: Vec<Album>,
}

#[derive(Serialize, Deserialize, Default)]
struct Store {
    #[serde(default)]
    days: Vec<Day>,
}

#[derive(Serialize)]
pub struct Today {
    pub date: String,
    pub albums: Vec<Album>,
    /// true = 直接用本地这份，没有联网
    pub cached: bool,
    /// true = 池子和网络都没成，退回了上一次成功的那天
    pub stale: bool,
}

fn store_path(app: &AppHandle) -> Option<PathBuf> {
    app.path().app_data_dir().ok().map(|d| d.join("daily.json"))
}

fn load(app: &AppHandle) -> Store {
    let Some(path) = store_path(app) else { return Store::default() };
    let Ok(text) = fs::read_to_string(path) else { return Store::default() };
    let text = text.trim_start_matches('\u{feff}');
    serde_json::from_str::<Store>(text).unwrap_or_default()
}

fn save(app: &AppHandle, store: &Store) -> Result<(), String> {
    let Some(path) = store_path(app) else { return Err("找不到应用数据目录".into()) };
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string_pretty(store).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, text).map_err(|e| e.to_string())?;
    fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

/* ---------------- 日期 ---------------- */

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn day_index(date: &str) -> i64 {
    let y: i64 = date.get(0..4).and_then(|s| s.parse().ok()).unwrap_or(1970);
    let m: i64 = date.get(5..7).and_then(|s| s.parse().ok()).unwrap_or(1);
    let d: i64 = date.get(8..10).and_then(|s| s.parse().ok()).unwrap_or(1);
    days_from_civil(y, m, d)
}

fn day_md(date: &str) -> &str {
    if date.len() >= 10 { &date[5..10] } else { "" }
}

/* ---------------- 展示历史：60 天冷却 + 7 天艺人冷却 + 30 天分布 ---------------- */

struct History {
    ids: HashSet<String>,
    keys: HashSet<String>,
    /// 艺人 -> 最后一次出现的天序号
    artists: HashMap<String, i64>,
    /// 最近 30 天的流派 / 年代分布
    genres_30d: HashMap<&'static str, usize>,
    decades_30d: HashMap<u8, usize>,
    n_30d: usize,
}

impl History {
    fn build(store: &Store, today: &str) -> Self {
        let today_idx = day_index(today);
        let mut h = History {
            ids: HashSet::new(),
            keys: HashSet::new(),
            artists: HashMap::new(),
            genres_30d: HashMap::new(),
            decades_30d: HashMap::new(),
            n_30d: 0,
        };
        for day in &store.days {
            let idx = day_index(&day.date);
            let age = today_idx - idx;
            if age < 0 {
                continue;
            }
            let recent = age < KEEP_DAYS as i64;
            let last30 = age < 30;
            for a in &day.albums {
                if recent {
                    h.ids.insert(a.id.clone());
                    h.keys.insert(art_key(&a.artist, &a.title));
                }
                let ka = norm_base(&a.artist);
                let e = h.artists.entry(ka).or_insert(idx);
                if idx > *e {
                    *e = idx;
                }
                if last30 {
                    *h.genres_30d.entry(genre_group(&a.genre)).or_insert(0) += 1;
                    *h.decades_30d.entry(era_slot(year_of(&a.date))).or_insert(0) += 1;
                    h.n_30d += 1;
                }
            }
        }
        h
    }

    fn artist_last(&self, artist: &str) -> Option<i64> {
        self.artists.get(&norm_base(artist)).copied()
    }

    fn genre_share(&self, group: &str) -> f64 {
        if self.n_30d == 0 {
            return 0.0;
        }
        *self.genres_30d.get(group).unwrap_or(&0) as f64 / self.n_30d as f64
    }

    fn era_share(&self, slot: u8) -> f64 {
        if self.n_30d == 0 {
            return 0.0;
        }
        *self.decades_30d.get(&slot).unwrap_or(&0) as f64 / self.n_30d as f64
    }
}

/* ---------------- 流派分组与上下限 ---------------- */

fn genre_group(g: &str) -> &'static str {
    let g = g.to_lowercase();
    let has = |keys: &[&str]| keys.iter().any(|k| g.contains(k));
    if has(&["hip-hop", "hip hop", "hiphop", "rap", "嘻哈", "饒舌", "饶舌", "說唱", "说唱"]) {
        "hiphop"
    } else if has(&["r&b", "rnb", "soul", "灵魂", "靈魂", "節奏藍調", "节奏蓝调"]) {
        "rnb"
    } else if has(&[
        "electronic", "electronica", "dance", "house", "techno", "trance", "edm", "電子", "电子",
        "舞曲",
    ]) {
        "electronic"
    } else if has(&["alternative", "indie", "另類", "另类"]) {
        "alternative"
    } else if has(&["metal", "金屬", "金属"]) {
        "metal"
    } else if has(&["rock", "搖滾", "摇滚"]) {
        "rock"
    } else if has(&["jazz", "爵士"]) {
        "jazz"
    } else if has(&["classical", "opera", "古典", "歌劇", "歌剧"]) {
        "classical"
    } else if has(&["country", "folk", "bluegrass", "鄉村", "乡村", "民謠", "民谣"]) {
        "country"
    } else if has(&["pop", "流行"]) {
        "pop"
    } else {
        "other"
    }
}

fn genre_cap(group: &str) -> usize {
    match group {
        "pop" => 5,
        "rock" => 5,
        "hiphop" => 4,
        "rnb" => 3,
        "electronic" => 3,
        "alternative" => 3,
        "jazz" => 2,
        "country" => 2,
        "metal" => 2,
        "classical" => 2,
        _ => 3,
    }
}

/// 年代桶：0=<1970 1=70s 2=80s 3=90s 4=00s 5=10s 6=更晚/未知
fn era_slot(year: u16) -> u8 {
    match year {
        0 => 6,
        y if y < 1970 => 0,
        y if y < 1980 => 1,
        y if y < 1990 => 2,
        y if y < 2000 => 3,
        y if y < 2010 => 4,
        y if y < 2020 => 5,
        _ => 6,
    }
}

/* ---------------- 抽签 ---------------- */

fn tier_weight(tier: u8, explore: bool) -> f64 {
    match (tier, explore) {
        (3, false) => 40.0,
        (2, false) => 24.0,
        (1, false) => 9.0,
        (0, false) => 3.0,
        /* 探索位：只想要"没那么大众"的那两档 */
        (1, true) => 30.0,
        (0, true) => 12.0,
        _ => 0.0,
    }
}

struct Picker {
    taken: HashSet<usize>,
    artists: HashSet<String>,
    genres: HashMap<&'static str, usize>,
    keys: HashSet<String>,
}

impl Picker {
    fn new() -> Self {
        Picker {
            taken: HashSet::new(),
            artists: HashSet::new(),
            genres: HashMap::new(),
            keys: HashSet::new(),
        }
    }

    fn can_take(&self, a: &PoolAlbum, cap_genre: bool) -> bool {
        if self.artists.contains(&norm_base(&a.artist)) || self.keys.contains(&a.key) {
            return false;
        }
        if cap_genre {
            let g = genre_group(&a.genre);
            if *self.genres.get(g).unwrap_or(&0) >= genre_cap(g) {
                return false;
            }
        }
        true
    }

    fn take(&mut self, i: usize, a: &PoolAlbum) {
        self.taken.insert(i);
        self.artists.insert(norm_base(&a.artist));
        self.keys.insert(a.key.clone());
        *self.genres.entry(genre_group(&a.genre)).or_insert(0) += 1;
    }
}

/// 从候选下标里按权重抽一张；explore = 只抽 B/C 档
fn draw_one(
    cands: &[usize],
    pool: &[&PoolAlbum],
    hist: &History,
    rng: &mut Rng,
    picker: &Picker,
    explore: bool,
    cap_genre: bool,
) -> Option<usize> {
    let mut weights: Vec<(usize, f64)> = Vec::with_capacity(cands.len());
    for &i in cands {
        if picker.taken.contains(&i) {
            continue;
        }
        let a = pool[i];
        if !picker.can_take(a, cap_genre) {
            continue;
        }
        let tw = tier_weight(a.tier, explore);
        if tw <= 0.0 {
            continue;
        }
        /* 长期均衡：最近 30 天出现得多的流派 / 年代降权 */
        let bal = 1.0 / (1.0 + 2.0 * hist.genre_share(genre_group(&a.genre)))
            * 1.0 / (1.0 + 1.2 * hist.era_share(era_slot(a.year())));
        weights.push((i, tw * bal));
    }
    if weights.is_empty() {
        return None;
    }
    let total: f64 = weights.iter().map(|(_, w)| w).sum();
    let mut r = rng.unit() * total;
    for (i, w) in &weights {
        r -= w;
        if r <= 0.0 {
            return Some(*i);
        }
    }
    weights.last().map(|(i, _)| *i)
}

/// 经典那 7 张的年代分配：至少跨 3 个年代，单年代最多 3 张
fn decade_plan(rng: &mut Rng) -> Vec<u8> {
    for _ in 0..40 {
        let mut counts = [0usize; 6];
        let mut plan = Vec::with_capacity(N_CLASSIC);
        let mut guard = 0;
        while plan.len() < N_CLASSIC && guard < 200 {
            guard += 1;
            let slot = rng.below(6) as u8;
            if counts[slot as usize] >= 3 {
                continue;
            }
            counts[slot as usize] += 1;
            plan.push(slot);
        }
        let distinct = counts.iter().filter(|c| **c > 0).count();
        if plan.len() == N_CLASSIC && distinct >= 3 {
            return plan;
        }
    }
    vec![0, 1, 2, 3, 4, 5, 3]
}

fn to_album(a: &PoolAlbum) -> Album {
    Album {
        id: a.id.clone(),
        sf: a.sf.clone(),
        title: a.title.clone(),
        artist: a.artist.clone(),
        date: a.date.clone(),
        genre: a.genre.clone(),
        tracks: a.tracks,
        art: a.art.clone(),
        anniv: false,
    }
}

/// 从池子里挑今天这 20 张
fn select(pool_rows: &[PoolAlbum], hist: &History, today: &str, rng: &mut Rng) -> Vec<Album> {
    let today_idx = day_index(today);
    let now_year = year_of(today);
    let modern_from = now_year.saturating_sub(MODERN_YEARS);

    /* 冷却过滤：60 天内的专辑 / 封面不要，7 天内的歌手不要 */
    let mut cands: Vec<&PoolAlbum> = Vec::with_capacity(pool_rows.len());
    let mut by_key: HashMap<String, usize> = HashMap::new();
    for a in pool_rows {
        if a.art.is_empty() || a.id.is_empty() {
            continue;
        }
        if hist.ids.contains(&a.id) || hist.keys.contains(&a.key) {
            continue;
        }
        if let Some(last) = hist.artist_last(&a.artist) {
            if today_idx - last < ARTIST_COOLDOWN_DAYS {
                continue;
            }
        }
        /* 同一张封面只留分数最高的一条 */
        match by_key.get(&a.key) {
            Some(&j) if cands[j].score >= a.score => continue,
            _ => {
                by_key.insert(a.key.clone(), cands.len());
                cands.push(a);
            }
        }
    }
    if cands.is_empty() {
        return Vec::new();
    }

    let modern: Vec<usize> =
        (0..cands.len()).filter(|i| cands[*i].year() >= modern_from).collect();
    let classic: Vec<usize> = (0..cands.len())
        .filter(|i| {
            let y = cands[*i].year();
            y > 0 && y < modern_from
        })
        .collect();
    let decades: Vec<Vec<usize>> = (0..6)
        .map(|slot| {
            classic
                .iter()
                .copied()
                .filter(|i| era_slot(cands[*i].year()) == slot as u8)
                .collect()
        })
        .collect();
    let all: Vec<usize> = (0..cands.len()).collect();

    let mut picker = Picker::new();
    let mut out: Vec<Album> = Vec::with_capacity(PER_DAY);

    /* 1) 先抽经典 7 张（按年代分配）——放前面是为了别让 pop/rock 的名额先被现代位吃掉，
       这里只从经典里抽，抽不到就跳过这个名额，留给后面的混合位 */
    let plan = decade_plan(rng);
    for slot in plan {
        if out.len() >= N_CLASSIC {
            break;
        }
        let picked = draw_one(&decades[slot as usize], &cands, hist, rng, &picker, false, true)
            .or_else(|| draw_one(&classic, &cands, hist, rng, &picker, false, true));
        let Some(i) = picked else { break };
        picker.take(i, cands[i]);
        out.push(to_album(cands[i]));
    }

    /* 2) 现代 8 张 */
    while out.len() < N_CLASSIC + N_MODERN {
        let Some(i) = draw_one(&modern, &cands, hist, rng, &picker, false, true)
            .or_else(|| draw_one(&all, &cands, hist, rng, &picker, false, true))
        else {
            break;
        };
        picker.take(i, cands[i]);
        out.push(to_album(cands[i]));
    }

    /* 3) 补齐剩下的（混合位），其中 2 张留给"探索位" */
    let mixed_start = out.len();
    let mixed_total = PER_DAY - mixed_start;
    let explore_at = mixed_total.saturating_sub(N_EXPLORE);
    let mut filled = 0usize;
    while out.len() < PER_DAY {
        let explore = filled >= explore_at;
        let picked = draw_one(&all, &cands, hist, rng, &picker, explore, true)
            .or_else(|| draw_one(&all, &cands, hist, rng, &picker, false, true));
        let Some(i) = picked else { break };
        picker.take(i, cands[i]);
        out.push(to_album(cands[i]));
        filled += 1;
    }

    /* 4) 还差就松开流派上限再补 */
    while out.len() < PER_DAY {
        let Some(i) = draw_one(&all, &cands, hist, rng, &picker, false, false) else { break };
        picker.take(i, cands[i]);
        out.push(to_album(cands[i]));
    }

    /* 5) 今天发行的顶到第一张当头条，其余打乱 */
    let md = day_md(today);
    if !md.is_empty() && !out.is_empty() {
        if let Some(pos) = out.iter().position(|a| day_md(&a.date) == md) {
            let mut hero = out.remove(pos);
            hero.anniv = true;
            out.insert(0, hero);
        }
    }
    if out.len() > 1 {
        let head = out.remove(0);
        rng.shuffle(&mut out);
        out.insert(0, head);
    }
    out
}

/* ---------------- 池子还没建好时的兜底：现抓一批 ---------------- */

async fn fallback_fetch(date: &str, store: &Store) -> Vec<Album> {
    let used: HashSet<String> =
        store.days.iter().flat_map(|d| d.albums.iter().map(|a| a.id.clone())).collect();
    let rows = pool::fetch_fresh(20).await;
    let mut rng = Rng::new();
    let mut cands: Vec<PoolAlbum> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for (a, _rank) in rows {
        if used.contains(&a.id) || !seen.insert(a.id.clone()) {
            continue;
        }
        cands.push(a);
    }
    rng.shuffle(&mut cands);
    let mut out: Vec<Album> = Vec::new();
    let mut artists: HashSet<String> = HashSet::new();
    let mut genres: HashMap<&'static str, usize> = HashMap::new();
    for a in &cands {
        if out.len() >= PER_DAY {
            break;
        }
        if !artists.insert(norm_base(&a.artist)) {
            continue;
        }
        let g = genre_group(&a.genre);
        if *genres.get(g).unwrap_or(&0) >= genre_cap(g) {
            continue;
        }
        *genres.entry(g).or_insert(0) += 1;
        out.push(to_album(a));
    }
    /* 还是不够就放宽 */
    for a in &cands {
        if out.len() >= PER_DAY {
            break;
        }
        if !out.iter().any(|x| x.id == a.id) {
            out.push(to_album(a));
        }
    }
    let md = day_md(date);
    if !md.is_empty() {
        if let Some(pos) = out.iter().position(|a| day_md(&a.date) == md) {
            let mut hero = out.remove(pos);
            hero.anniv = true;
            out.insert(0, hero);
        }
    }
    out
}

/* ---------------- 命令 ---------------- */

/// 今天这一屏：本地有就直接给，没有就从池子里挑（池子太小就现抓）
#[tauri::command]
pub async fn daily_today(app: AppHandle, date: String) -> Result<Today, String> {
    let mut store = load(&app);
    if let Some(day) = store.days.iter().find(|d| d.date == date) {
        if !day.albums.is_empty() {
            pool::spawn_refresh_if_needed(app.clone());
            return Ok(Today { date, albums: day.albums.clone(), cached: true, stale: false });
        }
    }

    let rows = pool::load(&app);
    let hist = History::build(&store, &date);
    let mut albums = if rows.len() >= POOL_MIN {
        select(&rows, &hist, &date, &mut Rng::new())
    } else {
        Vec::new()
    };
    if albums.is_empty() {
        albums = fallback_fetch(&date, &store).await;
    }

    if albums.is_empty() {
        return match store.days.last() {
            Some(day) if !day.albums.is_empty() => Ok(Today {
                date,
                albums: day.albums.clone(),
                cached: true,
                stale: true,
            }),
            _ => Err("今天这 20 张没拉到，检查一下网络再重试".into()),
        };
    }

    store.days.retain(|d| d.date != date);
    store.days.push(Day { date: date.clone(), albums: albums.clone() });
    store.days.sort_by(|a, b| a.date.cmp(&b.date));
    if store.days.len() > KEEP_DAYS {
        let cut = store.days.len() - KEEP_DAYS;
        store.days.drain(0..cut);
    }
    let _ = save(&app, &store);

    /* 今天的已经给出来了，池子慢慢在后台攒 */
    pool::spawn_refresh_if_needed(app.clone());

    Ok(Today { date, albums, cached: false, stale: false })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_pool(modern: usize, classic: usize) -> Vec<PoolAlbum> {
        let mut out = Vec::new();
        for i in 0..modern {
            out.push(PoolAlbum {
                id: format!("m{i}"),
                sf: "us".into(),
                title: format!("Modern {i}"),
                artist: format!("Artist M{i}"),
                date: format!("202{}-05-05", i % 5),
                genre: ["Pop", "Hip-Hop/Rap", "Electronic", "Rock"][i % 4].into(),
                tracks: 10,
                art: "https://example.com/a.jpg".into(),
                key: format!("artist m{i}|modern {i}"),
                score: 60 + (i % 30) as u8,
                tier: (i % 4) as u8,
                ..Default::default()
            });
        }
        for i in 0..classic {
            let year = 1960 + (i % 55) as u16;
            out.push(PoolAlbum {
                id: format!("c{i}"),
                sf: "us".into(),
                title: format!("Classic {i}"),
                artist: format!("Artist C{i}"),
                date: format!("{year}-03-03"),
                genre: ["Rock", "Jazz", "Country", "Classical", "Pop"][i % 5].into(),
                tracks: 11,
                art: "https://example.com/b.jpg".into(),
                key: format!("artist c{i}|classic {i}"),
                classic: true,
                score: 50 + (i % 40) as u8,
                tier: (i % 4) as u8,
                ..Default::default()
            });
        }
        out
    }

    #[test]
    fn selection_shape() {
        let pool = fake_pool(220, 220);
        let hist = History::build(&Store::default(), "2026-09-17");
        let out = select(&pool, &hist, "2026-09-17", &mut Rng::new());
        println!("选出 {} 张", out.len());
        let now_year = year_of("2026-09-17");
        let modern = out.iter().filter(|a| year_of(&a.date) >= now_year - MODERN_YEARS).count();
        let mut slots: Vec<u8> = out.iter().map(|a| era_slot(year_of(&a.date))).collect();
        slots.sort_unstable();
        slots.dedup();
        let mut artists: Vec<String> = out.iter().map(|a| norm_base(&a.artist)).collect();
        artists.sort();
        let before = artists.len();
        artists.dedup();
        println!(
            "现代 {modern} 张，年代桶 {slots:?}，歌手去重 {before} → {}",
            artists.len()
        );
        for a in &out {
            println!("   {:<6} {} — {} ({})", year_of(&a.date), a.artist, a.title, a.genre);
        }
        assert_eq!(out.len(), PER_DAY, "没凑够 20 张");
        assert_eq!(artists.len(), before, "有歌手一天上了两张");
        assert!(modern >= N_MODERN, "现代位不够：{modern}");
        let classic = out.len() - modern;
        assert!(classic >= N_CLASSIC, "经典位不够：{classic}");
        assert!(slots.len() >= 3, "年代不够分散：{slots:?}");
    }

    #[test]
    fn selection_avoids_history() {
        let pool = fake_pool(60, 60);
        let mut store = Store::default();
        store.days.push(Day {
            date: "2026-09-16".into(),
            albums: pool.iter().take(20).map(to_album).collect(),
        });
        let hist = History::build(&store, "2026-09-17");
        let out = select(&pool, &hist, "2026-09-17", &mut Rng::new());
        let old: std::collections::HashSet<&str> = store.days[0].albums.iter().map(|a| a.id.as_str()).collect();
        let dup = out.iter().filter(|a| old.contains(a.id.as_str())).count();
        println!("选出 {} 张，与昨天重复 {dup} 张；历史冷却集 {} 个", out.len(), hist.ids.len());
        assert_eq!(dup, 0, "出现了 60 天内重复的专辑");
    }
}
