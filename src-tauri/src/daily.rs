// 每日一屏：不再内置策展池，改成每天联网随机抽 20 张。
//
// 规则（按需求定的）：
// - daily.json 只留最近 60 天，一天一条：{ date, albums }
// - 启动时先看今天这条在不在：在就直接用本地这份，不联网（秒开）
// - 不在才联网：从「店区 × 流派」的榜单里随机挑一批 feed 抓候选，剔掉最近 60 天
//   出现过的专辑，再随机抽 20 张（同一张只上一次，尽量一个歌手一张）
// - 断网又没缓存：退回最近成功的那一天（stale=true）；一次都没成功过才报错
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

/// 每天一屏 20 张
const PER_DAY: usize = 20;
/// 历史保留天数（去重就是对着这份历史比）
const KEEP_DAYS: usize = 60;
/// 候选里攒够这么多张没重复的就收手；不够才抓第二批
const ENOUGH: usize = 48;
/// 同一种风格一天最多上几张
const GENRE_CAP: usize = 3;
/// 播放量榜取前 60（按播放量排的，靠后一点的也还是热门）；老榜单只看前 25
const CAP_PLAYED: usize = 60;
const CAP_CHART: usize = 25;
const BATCH: [usize; 2] = [14, 10];

/// 主流市场，数字是权重（美英给得最多）——小国冷门市场全部不要
const MARKETS: [(&str, usize); 14] = [
    ("us", 8),
    ("gb", 6),
    ("jp", 4),
    ("de", 3),
    ("fr", 3),
    ("ca", 3),
    ("au", 3),
    ("tw", 3),
    ("it", 2),
    ("es", 2),
    ("nl", 2),
    ("se", 2),
    ("br", 2),
    ("mx", 2),
];
/// 分类型榜只给美/英开（这两个的分类榜最主流），每个榜给几份权重
const GENRE_MARKETS: [&str; 2] = ["us", "gb"];
const GENRE_WEIGHT: usize = 1;

/// 主流流派 id（区域小语种 / 演歌 / 圣歌 / 健身 / 卡拉OK 这类都去掉）
const GENRES: [u32; 11] = [2, 5, 7, 11, 14, 15, 17, 18, 20, 21, 23];

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
    /// 图床基址（不带尺寸段），界面自己拼 3000 / 1000 / 600 / 100
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
    /// true = 联网没成，退回了上一次成功的那天
    pub stale: bool,
}

fn store_path(app: &AppHandle) -> Option<PathBuf> {
    app.path().app_data_dir().ok().map(|d| d.join("daily.json"))
}

fn load(app: &AppHandle) -> Store {
    let Some(path) = store_path(app) else { return Store::default() };
    let Ok(text) = fs::read_to_string(path) else { return Store::default() };
    /* 有人用记事本改过就会有 BOM，先剥掉再解析 */
    let text = text.trim_start_matches('\u{feff}');
    serde_json::from_str::<Store>(text).unwrap_or_default()
}

fn save(app: &AppHandle, store: &Store) -> Result<(), String> {
    let Some(path) = store_path(app) else { return Err("找不到应用数据目录".into()) };
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string_pretty(store).map_err(|e| e.to_string())?;
    /* 临时文件 + rename，避免写一半断电留下坏文件 */
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, text).map_err(|e| e.to_string())?;
    fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

/// 够用就行的随机源（xorshift64*），省得为了随机数再拉一个 crate
struct Rng(u64);

impl Rng {
    fn new() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x2545F4914F6CDD1D);
        Rng(nanos ^ 0x9E3779B97F4A7C15 | 1)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 { 0 } else { (self.next() % n as u64) as usize }
    }
    /// [0, 1) 之间的小数
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = self.below(i + 1);
            items.swap(i, j);
        }
    }
}

/// 老榜单接口：总榜 / 分类型榜（能带出老专辑，但也有不少精选集）
fn chart_url(sf: &str, genre: Option<u32>) -> String {
    match genre {
        Some(g) => format!("https://itunes.apple.com/{sf}/rss/topalbums/limit=100/genre={g}/json"),
        None => format!("https://itunes.apple.com/{sf}/rss/topalbums/limit=100/json"),
    }
}

/// 播放量榜：Apple 按播放量排出来的「最热门专辑」，比榜单接口主流得多
fn played_url(sf: &str) -> String {
    format!("https://rss.marketingtools.apple.com/api/v2/{sf}/music/most-played/100/albums.json")
}

/// 一个抓取来源
#[derive(Clone, Copy)]
struct Source {
    sf: &'static str,
    genre: Option<u32>,
    played: bool,
    /// 只认这个来源的前 cap 名
    cap: usize,
}

/// 这批抓哪些源：主流市场的播放量榜为主，老榜单（含美/英分类型榜）做补充
fn plan(rng: &mut Rng, count: usize) -> Vec<Source> {
    let mut list: Vec<Source> = Vec::new();
    for (sf, w) in MARKETS {
        for _ in 0..w {
            list.push(Source { sf, genre: None, played: true, cap: CAP_PLAYED });
        }
        /* 每个市场再留一份总榜，老专辑、经典专辑是从这里来的 */
        list.push(Source { sf, genre: None, played: false, cap: CAP_CHART });
    }
    for sf in GENRE_MARKETS {
        for _ in 0..GENRE_WEIGHT {
            for g in GENRES {
                list.push(Source { sf, genre: Some(g), played: false, cap: CAP_CHART });
            }
        }
    }
    rng.shuffle(&mut list);
    list.truncate(count);
    list
}

/// 形如 170x170bb.png 的尺寸段
fn is_size_seg(seg: &str) -> bool {
    let Some((wh, rest)) = seg.split_once("bb.") else { return false };
    let Some((w, h)) = wh.split_once('x') else { return false };
    !rest.is_empty()
        && w.chars().all(|c| c.is_ascii_digit())
        && h.chars().all(|c| c.is_ascii_digit())
        && !w.is_empty()
        && !h.is_empty()
}

/// RSS 给的是 .../cover.jpg/170x170bb.png 这种带尺寸的地址，砍掉尺寸段拿到基址
fn art_base(url: &str) -> Option<String> {
    let (head, tail) = url.rsplit_once('/')?;
    if !is_size_seg(tail) || head.is_empty() {
        return None;
    }
    Some(head.to_string())
}

fn day_md(date: &str) -> &str {
    if date.len() >= 10 { &date[5..10] } else { "" }
}

/// 榜单里混着的卡拉OK / 致敬专辑封面很难看，直接扔掉
fn junk(a: &Album) -> bool {
    /* 标题里带这些的，基本是翻录盘 / 伴唱盘 */
    const BAD_TITLE: &[&str] = &[
        "karaoke",
        "tribute",
        "made popular by",
        "in the style of",
        "originally performed",
        "as made famous",
        "lullaby",
        "berceuse",
        "instrumental worship",
        "cover music",
        "relax mode",
        "christmas",
        "xmas",
        "brown noise",
        "white noise",
        "rain sounds",
        "nature sounds",
        "meditation",
        "relaxing",
        "sleep",
        "オルゴール",
        "眠れる",
        "睡眠",
        "ヒーリング",
        "癒し",
        "子守唄",
        "赤ちゃん",
        "儿童",
        "兒歌",
        "催眠",
        "白噪音",
        "lofi",
        "lo-fi",
        "study music",
        "study beats",
        "type beat",
        "beats to",
        "trap beats",
        "chill beats",
        "asmr",
    ];
    /* 分类里带这些的（各语言都算上），也不是我们要的封面 */
    const BAD_GENRE: &[&str] = &[
        "karaoke", "lullab", "children", "kids", "kinder", "enfant", "niño", "nino", "christmas",
        "navidad", "natal", "weihnacht", "fitness", "workout", "hörspiel", "hoerspiel",
        "チルドレン", "キッズ", "こども", "子供", "赤ちゃん", "オルゴール", "睡眠", "ヒーリング",
        "癒し", "동요", "자장가", "儿童", "兒歌", "催眠", "白噪音", "barn", "børn", "bambini",
        "niños",
    ];
    let t = a.title.to_lowercase();
    let g = a.genre.to_lowercase();
    /* 环境音 / 助眠音频：歌手名和标题都算上（"Som De Chuva"、"Regen Macher" 这种） */
    const BAD_ANY: &[&str] = &[
        "regengeräusch",
        "regen entspannung",
        "chuva",
        "trovoadas",
        "rain sounds",
        "rain and thunder",
        "rainfall",
        "lluvia",
        "sonidos de",
        "sounds of nature",
        "nature sounds",
        "brown noise",
        "white noise",
        "asmr",
    ];
    let both = format!("{} {}", a.artist.to_lowercase(), t);
    BAD_TITLE.iter().any(|b| t.contains(b))
        || BAD_GENRE.iter().any(|b| g.contains(b))
        || BAD_ANY.iter().any(|b| both.contains(b))
        || is_compilation(a)
        || various_artists(a)
}

/// 标题里带这些的，基本都是「精选集 / 合辑」
fn is_compilation(a: &Album) -> bool {
    /* 标点统一成空格、撇号去掉，好按"词"判断："Best Of" / "best-of" / "That's" 都能对上 */
    let no_quote: String = a.title.to_lowercase().replace(['\'', '’', '`'], "");
    let t: String = no_quote
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect();
    let t = t.split_whitespace().collect::<Vec<_>>().join(" ");

    const PHRASES: &[&str] = &[
        "greatest hits",
        "best of",
        "the best",
        "very best",
        "all time",
        "top hits",
        "now that s what i call",
        "the essential",
        "hits collection",
        "grandes exitos",
        "grandes éxitos",
        "los mejores",
        "lo mejor de",
        "coleccion",
        "colección",
        "antologia",
        "antología",
        "os maiores sucessos",
        "melhores",
        "coletanea",
        "coletânea",
        "das beste",
        "il meglio",
        "les meilleurs",
        "meilleur de",
        "number ones",
    ];
    const WORDS: &[&str] = &[
        "hits",
        "best",
        "gold",
        "essential",
        "collection",
        "anthology",
        "compilation",
        "playlist",
        "definitive",
        "singles",
        "tutto",
        "tutti",
        "successi",
        "sucessos",
        "exitos",
        "éxitos",
        "raccolta",
        "recopilatorio",
        "integral",
        "anthologie",
        "greatest",
    ];
    const CJK: &[&str] = &[
        "ベスト", "全曲集", "コンプリート", "精選", "精选", "合辑", "合輯", "精选集", "精選集",
        "金曲", "典藏",
    ];

    PHRASES.iter().any(|p| t.contains(p))
        || CJK.iter().any(|p| a.title.contains(p))
        || t.split_whitespace().any(|w| WORDS.contains(&w))
}

/// 演唱者字段是「群星」的，也是合辑
fn various_artists(a: &Album) -> bool {
    const VA: &[&str] = &[
        "various artists",
        "various",
        "verschiedene interpret",
        "verschillende artiesten",
        "divers interpr",
        "vários intérpretes",
        "varios artistas",
        "vários artistas",
        "multi-interprètes",
        "multi interpretes",
        "artisti vari",
        "artistes variés",
        "interpreti vari",
        "群星",
    ];
    let s = a.artist.to_lowercase();
    VA.iter().any(|v| s.contains(v))
}

fn parse_feed(json: &serde_json::Value, sf: &str) -> Vec<Album> {
    let Some(rows) = json.pointer("/feed/entry").and_then(|v| v.as_array()) else { return Vec::new() };
    let mut out = Vec::with_capacity(rows.len());
    for e in rows {
        let id = e.pointer("/id/attributes/im:id").and_then(|v| v.as_str()).unwrap_or("");
        let title = e.pointer("/im:name/label").and_then(|v| v.as_str()).unwrap_or("").trim();
        let artist = e.pointer("/im:artist/label").and_then(|v| v.as_str()).unwrap_or("").trim();
        let art = e
            .pointer("/im:image")
            .and_then(|v| v.as_array())
            .and_then(|a| a.last())
            .and_then(|i| i.get("label"))
            .and_then(|v| v.as_str())
            .and_then(art_base);
        let (Some(art), true) = (art, !id.is_empty() && !title.is_empty() && !artist.is_empty())
        else {
            continue;
        };
        let released = e.pointer("/im:releaseDate/label").and_then(|v| v.as_str()).unwrap_or("");
        out.push(Album {
            id: id.to_string(),
            sf: sf.to_string(),
            title: title.to_string(),
            artist: artist.to_string(),
            date: released.get(..10).unwrap_or("").to_string(),
            genre: e
                .pointer("/category/attributes/label")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            tracks: e
                .pointer("/im:itemCount/label")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse().ok())
                .unwrap_or(0),
            art,
            anniv: false,
        });
    }
    out
}

/// 播放量榜的结构（feed.results）跟老榜单不一样，单独解析
fn parse_played(json: &serde_json::Value, sf: &str) -> Vec<Album> {
    let Some(rows) = json.pointer("/feed/results").and_then(|v| v.as_array()) else { return Vec::new() };
    let mut out = Vec::with_capacity(rows.len());
    for e in rows {
        let id = e.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let title = e.get("name").and_then(|v| v.as_str()).unwrap_or("").trim();
        let artist = e.get("artistName").and_then(|v| v.as_str()).unwrap_or("").trim();
        let art = e.get("artworkUrl100").and_then(|v| v.as_str()).and_then(art_base);
        let (Some(art), true) = (art, !id.is_empty() && !title.is_empty() && !artist.is_empty())
        else {
            continue;
        };
        let released = e.get("releaseDate").and_then(|v| v.as_str()).unwrap_or("");
        out.push(Album {
            id: id.to_string(),
            sf: sf.to_string(),
            title: title.to_string(),
            artist: artist.to_string(),
            date: released.get(..10).unwrap_or("").to_string(),
            genre: e
                .pointer("/genres/0/name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            tracks: 0, /* 这个接口不给曲目数，翻到背面会现查 */
            art,
            anniv: false,
        });
    }
    out
}

async fn fetch_source(client: &reqwest::Client, src: Source) -> Vec<Album> {
    let url = if src.played { played_url(src.sf) } else { chart_url(src.sf, src.genre) };
    let Ok(res) = client.get(&url).send().await else { return Vec::new() };
    if !res.status().is_success() {
        return Vec::new();
    }
    let Ok(json) = res.json::<serde_json::Value>().await else { return Vec::new() };
    if src.played {
        parse_played(&json, src.sf)
    } else {
        parse_feed(&json, src.sf)
    }
}

/// 一个候选：除了专辑本身，还记着它在榜上的最好名次、被几个榜收录过
struct Cand {
    album: Album,
    hits: u32,
    rank: usize,
}

/// 名次越靠前、被越多榜单收录 → 抽中的概率越大（这就是"主流"的量化）
fn cand_weight(c: &Cand) -> f64 {
    (c.hits as f64) / (1.0 + c.rank as f64 / 8.0)
}

/// 按权重抽一个下标
fn weighted_index(cands: &[Cand], rng: &mut Rng) -> usize {
    let total: f64 = cands.iter().map(cand_weight).sum();
    let mut r = rng.unit() * total;
    for (i, c) in cands.iter().enumerate() {
        r -= cand_weight(c);
        if r <= 0.0 {
            return i;
        }
    }
    cands.len() - 1
}

/// 从新鲜候选里挑 20 张：按名次加权抽，一个歌手一天只上一张、一种风格最多三张，
/// 凑不够再放宽；候选里如果有今天发行的，顶到第一张当头条（角标「发行纪念日」）
fn pick(mut cands: Vec<Cand>, date: &str, rng: &mut Rng) -> Vec<Album> {
    if cands.is_empty() {
        return Vec::new();
    }

    let mut out: Vec<Album> = Vec::with_capacity(PER_DAY);
    let md = day_md(date);
    if !md.is_empty() {
        if let Some(i) = cands.iter().position(|c| day_md(&c.album.date) == md) {
            let mut hero = cands.remove(i).album;
            hero.anniv = true;
            out.push(hero);
        }
    }

    let mut artists: HashSet<String> = out.iter().map(|a| a.artist.to_lowercase()).collect();
    let mut genres: HashMap<String, usize> = HashMap::new();
    for a in &out {
        *genres.entry(a.genre.to_lowercase()).or_insert(0) += 1;
    }

    let mut blocked: Vec<Cand> = Vec::new();
    while out.len() < PER_DAY && !cands.is_empty() {
        let c = cands.remove(weighted_index(&cands, rng));
        let genre = c.album.genre.to_lowercase();
        if genres.get(&genre).copied().unwrap_or(0) >= GENRE_CAP {
            blocked.push(c); /* 同一种风格一天最多三张，免得一屏全是乡村 */
            continue;
        }
        if !artists.insert(c.album.artist.to_lowercase()) {
            blocked.push(c);
            continue;
        }
        *genres.entry(genre).or_insert(0) += 1;
        out.push(c.album);
    }

    /* 实在凑不够就放宽：先吃刚被风格 / 歌手挡下来的，再吃名次靠后的 */
    let mut have: HashSet<String> = out.iter().map(|a| a.id.clone()).collect();
    for c in blocked.into_iter().chain(cands) {
        if out.len() >= PER_DAY {
            break;
        }
        if have.insert(c.album.id.clone()) {
            out.push(c.album);
        }
    }

    if out.len() > 1 {
        rng.shuffle(&mut out[1..]); /* 头条留着，其余打乱 */
    }
    out
}

/// 抓一整天：凑够 ENOUGH 张没重复的就停，最多抓两批
async fn fetch_day(date: &str, used: &HashSet<String>) -> Vec<Album> {
    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .user_agent("CoverArt/0.1 (+https://github.com/duanshuiliuliuliu/CoverArt)")
        .build()
    else {
        return Vec::new();
    };

    let mut rng = Rng::new();
    let mut fresh: Vec<Cand> = Vec::new();
    let mut seen: HashMap<String, usize> = HashMap::new();

    for batch in BATCH {
        let sources = plan(&mut rng, batch);
        let mut handles = Vec::with_capacity(batch);
        for src in sources.iter().copied() {
            let client = client.clone();
            handles.push(tauri::async_runtime::spawn(async move {
                fetch_source(&client, src).await
            }));
        }
        for (i, h) in handles.into_iter().enumerate() {
            let cap = sources[i].cap;
            let Ok(rows) = h.await else { continue };
            for (rank, a) in rows.into_iter().enumerate() {
                /* 名次太靠后的不要：这是"主流"的第一道闸 */
                if rank >= cap {
                    break;
                }
                /* 最近 60 天出现过的不算；精选集合辑、翻录盘也不要 */
                if used.contains(&a.id) || junk(&a) {
                    continue;
                }
                match seen.get(&a.id) {
                    /* 被多个榜收录：命中次数 +1，取最好的名次 */
                    Some(&i) => {
                        fresh[i].hits += 1;
                        fresh[i].rank = fresh[i].rank.min(rank);
                    }
                    None => {
                        seen.insert(a.id.clone(), fresh.len());
                        fresh.push(Cand { album: a, hits: 1, rank });
                    }
                }
            }
        }
        if fresh.len() >= ENOUGH {
            break;
        }
    }

    pick(fresh, date, &mut rng)
}

/// 今天这一屏：本地有就直接给，没有才联网抓
#[tauri::command]
pub async fn daily_today(app: AppHandle, date: String) -> Result<Today, String> {
    let mut store = load(&app);
    if let Some(day) = store.days.iter().find(|d| d.date == date) {
        if !day.albums.is_empty() {
            return Ok(Today { date, albums: day.albums.clone(), cached: true, stale: false });
        }
    }

    let used: HashSet<String> =
        store.days.iter().flat_map(|d| d.albums.iter().map(|a| a.id.clone())).collect();
    let albums = fetch_day(&date, &used).await;

    if albums.is_empty() {
        /* 联网没成：退回最近成功的那一天，画面不至于空白 */
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

    Ok(Today { date, albums, cached: false, stale: false })
}
