// 候选池：把「当下主流」（Apple 榜单）和「历史经典」（Wikidata 年代榜）攒成本地池子，
// 每天从池子里挑 20 张（挑法在 daily.rs）。
//
// 为什么要池子：每天现拉只能拉到"当下在榜"的专辑，历史专辑没有落脚点；
// 攒成池子以后，主流度、年代比例、流派比例、60 天不重复都变成可控制的量。
//
// 约定：
// - pool.json 只存合格专辑（过滤 + 打分后的），目标 TARGET 张（第一版 5000）；
// - 建池是后台增量的：每次启动补一批，界面不等它；
// - 展示历史不在这个文件里，daily.json 才是（最近 60 天）。
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

/// 池子目标张数（用户定的第一版）
pub const TARGET: usize = 5000;
/// 池子还小的时候多抓一点，够大了就只做小幅更新
const APPLE_BATCH_SMALL: usize = 20;
const APPLE_BATCH_FULL: usize = 12;
/// 池子满了以后多久还去补一次新歌
const TOPUP_SECS: u64 = 7 * 24 * 3600;
/// 年代榜多久重拉一次
const ERA_TTL_SECS: u64 = 30 * 24 * 3600;
/// Wikipedia 清单多久重拉一次
const WIKI_TTL_SECS: u64 = 60 * 24 * 3600;
/// 一次刷新最多拉几页 Wikipedia 清单
const WIKI_FETCH_BUDGET: usize = 2;
/// 年代榜每个年代取多少条
const ERA_LIMIT: usize = 60;
/// 维基语言版本数低于这个的不算"经典"（等于知名度门槛）
const ERA_MIN_LINKS: u32 = 15;
/// 一次刷新最多解析多少个歌手的旧作
const ARTIST_BUDGET_SMALL: usize = 40;
const ARTIST_BUDGET_FULL: usize = 8;
/// 一次刷新最多拉几个年代（每个 Wikidata 查询 5~10 秒）
const ERA_FETCH_BUDGET: usize = 1;

/// 播放量榜取前 60；老榜单只看前 25
const CAP_PLAYED: usize = 60;
const CAP_CHART: usize = 25;

/// 主流市场：(店区, 权重, 有没有总榜接口)
const MARKETS: [(&str, usize, bool); 17] = [
    ("us", 8, true),
    ("gb", 6, true),
    ("cn", 6, false),
    ("jp", 4, true),
    ("de", 3, true),
    ("fr", 3, true),
    ("ca", 3, true),
    ("au", 3, true),
    ("tw", 3, true),
    ("hk", 3, true),
    ("sg", 2, true),
    ("it", 2, true),
    ("es", 2, true),
    ("nl", 2, true),
    ("se", 2, true),
    ("br", 2, true),
    ("mx", 2, true),
];
/// 分类型榜只给美/英开
const GENRE_MARKETS: [&str; 2] = ["us", "gb"];
const GENRES: [u32; 11] = [2, 5, 7, 11, 14, 15, 17, 18, 20, 21, 23];

/// 年代榜覆盖的区间
const DECADES: [(&str, u32, u32); 7] = [
    ("1950s", 1950, 1959),
    ("1960s", 1960, 1969),
    ("1970s", 1970, 1979),
    ("1980s", 1980, 1989),
    ("1990s", 1990, 1999),
    ("2000s", 2000, 2009),
    ("2010s", 2010, 2019),
];

/* ---------------- 文本归一化（去重、指纹都靠它） ---------------- */

pub fn year_of(date: &str) -> u16 {
    date.get(..4).and_then(|s| s.parse().ok()).unwrap_or(0)
}

/// 小写 + 去掉括号里那串版本说明 + 去标点，空格归一
pub fn norm_text(s: &str) -> String {
    let lower = s.to_lowercase();
    let mut out = String::with_capacity(lower.len());
    let mut depth: u32 = 0;
    for ch in lower.chars() {
        match ch {
            '(' | '（' | '[' | '【' => {
                depth += 1;
                out.push(' ');
            }
            ')' | '）' | ']' | '】' => {
                depth = depth.saturating_sub(1);
                out.push(' ');
            }
            _ if depth > 0 => {}
            c if c.is_alphanumeric() => out.push(c),
            _ => out.push(' '),
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 再砍掉 remaster / deluxe 这类词 —— 同一张专辑的不同版本要归成一个"封面指纹"
pub fn norm_base(s: &str) -> String {
    const DROP: &[&str] = &[
        "remaster", "remastered", "deluxe", "edition", "version", "mix", "anniversary",
        "expanded", "super", "bonus", "reissue", "digitally",
        /* 现场 / 巡演 / 精选版本要和原版归成同一张封面 */
        "live", "tour", "concert", "sessions", "bootleg", "remix", "remixes",
    ];
    norm_text(s)
        .split_whitespace()
        .filter(|w| !DROP.contains(w))
        .collect::<Vec<_>>()
        .join(" ")
}

/// 封面指纹：艺人 + 归一化标题。同一个指纹的专辑（原版/重制/豪华版）算"同一张封面"
pub fn art_key(artist: &str, title: &str) -> String {
    format!("{}|{}", norm_base(artist), norm_base(title))
}

/* ---------------- 小随机源 ---------------- */

/* ---------------- HTTP 客户端 ----------------
   这台机器只有 link-local IPv6，而 wikidata / Apple 都带 AAAA 记录：
   tokio 会先连 IPv6、连不上再干等到超时，表现就是"有时候能拉到、有时候超时"
   （curl 会走 Happy Eyeballs，所以一直是通的）。
   这里在建客户端时先把域名解析成 IPv4 钉住，绕开 IPv6。 */
const PINNED_HOSTS: [&str; 5] = [
    "query.wikidata.org",
    "itunes.apple.com",
    "rss.marketingtools.apple.com",
    "music.apple.com",
    "is1-ssl.mzstatic.com",
];

fn resolve_ipv4(host: &str) -> Option<std::net::IpAddr> {
    (host, 443)
        .to_socket_addrs()
        .ok()?
        .find(|a| a.is_ipv4())
        .map(|a| a.ip())
}

/// 统一用这个建客户端：超时 + UA + 只走 IPv4
pub fn http_client(timeout_secs: u64) -> Option<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .user_agent("CoverArt/0.1 (+https://github.com/duanshuiliuliuliu/CoverArt)");
    for host in PINNED_HOSTS {
        if let Some(ip) = resolve_ipv4(host) {
            builder = builder.resolve(host, SocketAddr::new(ip, 443));
        }
    }
    builder.build().ok()
}

pub struct Rng(u64);

impl Rng {
    pub fn new() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x2545F4914F6CDD1D);
        Rng(nanos ^ 0x9E3779B97F4A7C15 | 1)
    }
    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
    pub fn below(&mut self, n: usize) -> usize {
        if n == 0 { 0 } else { (self.next() % n as u64) as usize }
    }
    pub fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = self.below(i + 1);
            items.swap(i, j);
        }
    }
}

/* ---------------- 质量过滤 ---------------- */

/// 精选集：标题里带这些的都是
pub fn is_compilation(title: &str) -> bool {
    let no_quote: String = title.to_lowercase().replace(['\'', '’', '`'], "");
    let t = norm_text(&no_quote);
    const PHRASES: &[&str] = &[
        "greatest hits", "best of", "the best", "very best", "all time", "top hits",
        "now that s what i call", "the essential", "hits collection", "grandes exitos",
        "grandes éxitos", "los mejores", "lo mejor de", "coleccion", "colección", "antologia",
        "antología", "os maiores sucessos", "melhores", "coletanea", "coletânea", "das beste",
        "il meglio", "les meilleurs", "meilleur de", "number ones", "the singles",
    ];
    const WORDS: &[&str] = &[
        "hits", "best", "gold", "essential", "collection", "anthology", "compilation",
        "playlist", "definitive", "singles", "tutto", "tutti", "successi", "sucessos", "exitos",
        "éxitos", "raccolta", "recopilatorio", "integral", "anthologie", "greatest",
    ];
    const CJK: &[&str] = &[
        "ベスト", "全曲集", "コンプリート", "精選", "精选", "合辑", "合輯", "精选集", "精選集",
        "金曲", "典藏", "自選集", "自选集", "作品集", "精選輯",
    ];
    PHRASES.iter().any(|p| t.contains(p))
        || CJK.iter().any(|p| title.contains(p))
        || t.split_whitespace().any(|w| WORDS.contains(&w))
}

fn various_artists(artist: &str) -> bool {
    const VA: &[&str] = &[
        "various artists", "various", "verschiedene interpret", "verschillende artiesten",
        "divers interpr", "vários intérpretes", "varios artistas", "vários artistas",
        "multi-interprètes", "multi interpretes", "artisti vari", "artistes variés",
        "interpreti vari", "群星",
    ];
    let s = artist.to_lowercase();
    VA.iter().any(|v| s.contains(v))
}

/// 这条数据要不要（不合格就整个丢掉）
pub fn junk(id: &str, title: &str, artist: &str, genre: &str, art: &str, tracks: u32) -> bool {
    if id.is_empty() || title.trim().is_empty() || artist.trim().is_empty() || art.is_empty() {
        return true;
    }
    /* 单曲 / 无曲目信息的不算专辑封面 */
    if tracks > 0 && tracks < 3 {
        return true;
    }
    /* 标题里带这些的：翻录盘、伴唱盘、助眠音频 */
    const BAD_TITLE: &[&str] = &[
        "karaoke", "tribute", "made popular by", "in the style of", "originally performed",
        "as made famous", "lullaby", "berceuse", "instrumental worship", "cover music",
        "relax mode", "christmas", "xmas", "brown noise", "white noise", "rain sounds",
        "nature sounds", "meditation", "relaxing", "sleep", "lofi", "lo-fi", "study music",
        "study beats", "type beat", "beats to", "trap beats", "chill beats", "asmr", "オルゴール",
        "眠れる", "睡眠", "ヒーリング", "癒し", "子守唄", "赤ちゃん", "儿童", "兒歌", "催眠",
        "白噪音", "寶寶", "宝宝", "搖籃曲", "摇篮曲", "安眠", "床邊音樂", "床边音乐", "哄睡",
        "助眠", "bootleg", "remix", "remixes", "demos", "outtakes",
    ];
    /* 分类里带这些的（各语言都算上） */
    const BAD_GENRE: &[&str] = &[
        "karaoke", "lullab", "children", "kids", "kinder", "enfant", "niño", "nino", "christmas",
        "navidad", "natal", "weihnacht", "fitness", "workout", "hörspiel", "hoerspiel",
        "チルドレン", "キッズ", "こども", "子供", "赤ちゃん", "オルゴール", "睡眠", "ヒーリング",
        "癒し", "동요", "자장가", "儿童", "兒歌", "催眠", "白噪音", "barn", "børn", "bambini",
        "niños",
    ];
    /* 环境音 / 助眠音频：歌手名和标题一起看 */
    const BAD_ANY: &[&str] = &[
        "regengeräusch", "regen entspannung", "chuva", "trovoadas", "rain sounds",
        "rain and thunder", "rainfall", "lluvia", "sonidos de", "sounds of nature",
        "nature sounds", "brown noise", "white noise", "asmr",
    ];

    let t = title.to_lowercase();
    let g = genre.to_lowercase();
    let both = format!("{} {}", artist.to_lowercase(), t);
    BAD_TITLE.iter().any(|b| t.contains(b))
        || BAD_GENRE.iter().any(|b| g.contains(b))
        || BAD_ANY.iter().any(|b| both.contains(b))
        || various_artists(artist)
        || is_compilation(title)
}

/* ---------------- 池子里的专辑 ---------------- */

/// 没上过榜的名次占位
const NO_RANK: u32 = u32::MAX;

#[derive(Clone, Serialize, Deserialize, Default)]
pub struct PoolAlbum {
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
    /// Apple 的 artistId，用来分辨重名歌手
    #[serde(default)]
    pub artist_id: String,
    /// 封面指纹（艺人 + 归一化标题）
    #[serde(default)]
    pub key: String,
    /// 见过的最好名次
    #[serde(default = "no_rank")]
    pub rank: u32,
    /// 被几个店区 / 榜单收录过
    #[serde(default)]
    pub regions: u32,
    /// 历史知名度（维基语言版本数）
    #[serde(default)]
    pub recognition: u32,
    /// 0~100 主流度
    #[serde(default)]
    pub score: u8,
    /// 3=S 2=A 1=B 0=C
    #[serde(default)]
    pub tier: u8,
    /// 来自年代经典池
    #[serde(default)]
    pub classic: bool,
    /// 来源标记（调试用）
    #[serde(default)]
    pub src: Vec<String>,
    /// 最后一次被刷新看到的时间
    #[serde(default)]
    pub seen: u64,
}

fn no_rank() -> u32 {
    NO_RANK
}

impl PoolAlbum {
    pub fn year(&self) -> u16 {
        year_of(&self.date)
    }
    fn touch(&mut self, src: String) {
        if !self.src.contains(&src) && self.src.len() < 5 {
            self.src.push(src);
        }
        self.seen = now();
    }
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct EraEntry {
    pub decade: String,
    pub artist: String,
    pub title: String,
    pub year: u16,
    pub links: u32,
}

#[derive(Serialize, Deserialize, Default)]
struct PoolFile {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    updated: u64,
    #[serde(default)]
    albums: Vec<PoolAlbum>,
    /// 年代榜：文件缓存 + 每个年代最后一次拉取时间
    #[serde(default)]
    era_list: Vec<EraEntry>,
    #[serde(default)]
    eras: BTreeMap<String, u64>,
    /// Wikipedia 清单页：最后一次拉取时间
    #[serde(default)]
    wiki: BTreeMap<String, u64>,
    /// 已经解析过旧作的歌手（名字 -> 时间）
    #[serde(default)]
    artists: BTreeMap<String, u64>,
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn pool_path(app: &AppHandle) -> Option<PathBuf> {
    app.path().app_data_dir().ok().map(|d| d.join("pool.json"))
}

pub fn load(app: &AppHandle) -> Vec<PoolAlbum> {
    let Some(path) = pool_path(app) else { return Vec::new() };
    let Ok(text) = fs::read_to_string(path) else { return Vec::new() };
    let text = text.trim_start_matches('\u{feff}');
    serde_json::from_str::<PoolFile>(text).map(|f| f.albums).unwrap_or_default()
}

fn load_file(app: &AppHandle) -> PoolFile {
    let Some(path) = pool_path(app) else { return PoolFile::default() };
    let Ok(text) = fs::read_to_string(path) else { return PoolFile::default() };
    let text = text.trim_start_matches('\u{feff}');
    serde_json::from_str::<PoolFile>(text).unwrap_or_default()
}

fn save(app: &AppHandle, file: &PoolFile) -> Result<(), String> {
    let Some(path) = pool_path(app) else { return Err("找不到应用数据目录".into()) };
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string(file).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, text).map_err(|e| e.to_string())?;
    fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

/* ---------------- 主流度打分 ---------------- */

/// 榜单覆盖(40) + 跨市场(20) + 历史知名度(25) + 时间活跃度(15)
fn rescore(file: &mut PoolFile) {
    let this_year = year_of(&iso_today());
    for a in file.albums.iter_mut() {
        let mut s = 0.0f64;
        if a.rank != NO_RANK {
            let r = a.rank as f64;
            s += if r < 5.0 { 40.0 } else { (40.0 - r * 0.55).max(6.0) };
        }
        s += (a.regions.min(6) as f64 / 6.0) * 20.0;
        s += (a.recognition.min(60) as f64 / 60.0) * 25.0;
        let y = a.year();
        if y > 0 {
            s += if y + 1 >= this_year { 15.0 } else if y + 5 >= this_year { 10.0 } else if y + 15 >= this_year { 5.0 } else { 0.0 };
        }
        a.score = s.min(100.0) as u8;
        a.tier = match a.score {
            75..=255 => 3,
            55..=74 => 2,
            35..=54 => 1,
            _ => 0,
        };
        if a.key.is_empty() {
            a.key = art_key(&a.artist, &a.title);
        }
    }
}

fn iso_today() -> String {
    /* 只用来算"今年"，UTC 差一天无所谓 */
    let secs = now();
    let days = secs / 86400;
    let mut y = 1970i64;
    let mut d = days as i64;
    loop {
        let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
        let len = if leap { 366 } else { 365 };
        if d < len {
            break;
        }
        d -= len;
        y += 1;
    }
    format!("{y:04}-01-01")
}

/* ---------------- Apple 榜单：抓 + 解析 ---------------- */

fn art_base(url: &str) -> Option<String> {
    let (head, tail) = url.rsplit_once('/')?;
    let (wh, rest) = tail.split_once("bb.")?;
    let (w, h) = wh.split_once('x')?;
    let ok = !rest.is_empty()
        && !w.is_empty()
        && !h.is_empty()
        && w.chars().all(|c| c.is_ascii_digit())
        && h.chars().all(|c| c.is_ascii_digit());
    if !ok || head.is_empty() {
        return None;
    }
    Some(head.to_string())
}

fn chart_url(sf: &str, genre: Option<u32>) -> String {
    match genre {
        Some(g) => format!("https://itunes.apple.com/{sf}/rss/topalbums/limit=100/genre={g}/json"),
        None => format!("https://itunes.apple.com/{sf}/rss/topalbums/limit=100/json"),
    }
}

fn played_url(sf: &str) -> String {
    format!("https://rss.marketingtools.apple.com/api/v2/{sf}/music/most-played/100/albums.json")
}

#[derive(Clone, Copy)]
struct Source {
    sf: &'static str,
    genre: Option<u32>,
    played: bool,
    cap: usize,
}

fn plan(rng: &mut Rng, count: usize) -> Vec<Source> {
    let mut list: Vec<Source> = Vec::new();
    for (sf, w, has_chart) in MARKETS {
        for _ in 0..w {
            list.push(Source { sf, genre: None, played: true, cap: CAP_PLAYED });
        }
        if has_chart {
            list.push(Source { sf, genre: None, played: false, cap: CAP_CHART });
        }
    }
    for sf in GENRE_MARKETS {
        for g in GENRES {
            list.push(Source { sf, genre: Some(g), played: false, cap: CAP_CHART });
        }
    }
    rng.shuffle(&mut list);
    list.truncate(count);
    list
}

fn parse_chart(json: &serde_json::Value, sf: &str) -> Vec<PoolAlbum> {
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
        let Some(art) = art else { continue };
        let released = e.pointer("/im:releaseDate/label").and_then(|v| v.as_str()).unwrap_or("");
        let tracks = e
            .pointer("/im:itemCount/label")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let genre = e
            .pointer("/category/attributes/label")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if junk(id, title, artist, genre, &art, tracks) {
            continue;
        }
        out.push(PoolAlbum {
            id: id.to_string(),
            sf: sf.to_string(),
            title: title.to_string(),
            artist: artist.to_string(),
            date: released.get(..10).unwrap_or("").to_string(),
            genre: genre.to_string(),
            tracks,
            key: art_key(artist, title),
            art,
            ..Default::default()
        });
    }
    out
}

fn parse_played(json: &serde_json::Value, sf: &str) -> Vec<PoolAlbum> {
    let Some(rows) = json.pointer("/feed/results").and_then(|v| v.as_array()) else { return Vec::new() };
    let mut out = Vec::with_capacity(rows.len());
    for e in rows {
        let id = e.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let title = e.get("name").and_then(|v| v.as_str()).unwrap_or("").trim();
        let artist = e.get("artistName").and_then(|v| v.as_str()).unwrap_or("").trim();
        let art = e.get("artworkUrl100").and_then(|v| v.as_str()).and_then(art_base);
        let Some(art) = art else { continue };
        let released = e.get("releaseDate").and_then(|v| v.as_str()).unwrap_or("");
        let genre = e.pointer("/genres/0/name").and_then(|v| v.as_str()).unwrap_or("");
        if junk(id, title, artist, genre, &art, 0) {
            continue;
        }
        out.push(PoolAlbum {
            id: id.to_string(),
            sf: sf.to_string(),
            title: title.to_string(),
            artist: artist.to_string(),
            date: released.get(..10).unwrap_or("").to_string(),
            genre: genre.to_string(),
            tracks: 0,
            key: art_key(artist, title),
            art,
            artist_id: e.get("artistId").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            ..Default::default()
        });
    }
    out
}

async fn fetch_source(client: &reqwest::Client, src: Source) -> Vec<PoolAlbum> {
    let url = if src.played { played_url(src.sf) } else { chart_url(src.sf, src.genre) };
    let Ok(res) = client.get(&url).send().await else { return Vec::new() };
    if !res.status().is_success() {
        return Vec::new();
    }
    let Ok(json) = res.json::<serde_json::Value>().await else { return Vec::new() };
    if src.played {
        parse_played(&json, src.sf)
    } else {
        parse_chart(&json, src.sf)
    }
}

/// 抓一批 Apple 榜单（并发，每个源只取前 cap 名）
async fn fetch_apple(client: &reqwest::Client, rng: &mut Rng, count: usize) -> Vec<(PoolAlbum, usize)> {
    let sources = plan(rng, count);
    let mut out = Vec::new();
    /* 一次最多 8 个并发：一口气发 20 个会被 Apple 限速，反而更慢 */
    for chunk in sources.chunks(8) {
        let mut handles = Vec::with_capacity(chunk.len());
        for src in chunk.iter().copied() {
            let client = client.clone();
            handles.push(tauri::async_runtime::spawn(async move { fetch_source(&client, src).await }));
        }
        for (i, h) in handles.into_iter().enumerate() {
            let cap = chunk[i].cap;
            let Ok(rows) = h.await else { continue };
            for (rank, a) in rows.into_iter().enumerate() {
                if rank >= cap {
                    break;
                }
                out.push((a, rank));
            }
        }
    }
    out
}

/* ---------------- Wikidata 年代榜 ---------------- */

fn era_query(from: u32, to: u32) -> String {
    format!(
        "SELECT ?albumLabel ?artistLabel ?date ?links WHERE {{
  ?album wdt:P31/wdt:P279* wd:Q482994 ; wdt:P577 ?date ; wdt:P175 ?artist ; wikibase:sitelinks ?links .
  FILTER(YEAR(?date) >= {from} && YEAR(?date) <= {to} && ?links > {ERA_MIN_LINKS})
  SERVICE wikibase:label {{ bd:serviceParam wikibase:language \"en\". }}
}} ORDER BY DESC(?links) LIMIT {ERA_LIMIT}"
    )
}

async fn fetch_era(client: &reqwest::Client, decade: &str, from: u32, to: u32) -> Vec<EraEntry> {
    let url = format!(
        "https://query.wikidata.org/sparql?format=json&query={}",
        urlencode(&era_query(from, to))
    );
    /* 这条链路偶发卡住（连接被中间设备掐掉），所以给三次机会 */
    let mut json = None;
    for attempt in 0..2 {
        if attempt > 0 {
            pause(400 * attempt).await;
        }
        let Ok(res) = client.get(&url).header("Accept", "application/sparql-results+json").send().await
        else {
            continue;
        };
        if !res.status().is_success() {
            continue;
        }
        if let Ok(v) = res.json::<serde_json::Value>().await {
            json = Some(v);
            break;
        }
    }
    let Some(json) = json else { return Vec::new() };
    let Some(rows) = json.pointer("/results/bindings").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for r in rows {
        let get = |k: &str| r.pointer(&format!("/{k}/value")).and_then(|v| v.as_str()).unwrap_or("");
        let artist = get("artistLabel");
        let title = get("albumLabel");
        let date = get("date");
        let links: u32 = get("links").parse().unwrap_or(0);
        let fp = art_key(artist, title);
        if artist.is_empty() || title.is_empty() || !seen.insert(fp) {
            continue;
        }
        out.push(EraEntry {
            decade: decade.to_string(),
            artist: artist.to_string(),
            title: title.to_string(),
            year: year_of(date),
            links,
        });
    }
    out
}

/// 异步等一会儿（不占着 tokio 的工作线程）
async fn pause(ms: u64) {
    let _ = tauri::async_runtime::spawn_blocking(move || {
        std::thread::sleep(Duration::from_millis(ms));
    })
    .await;
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/* ---------------- Wikipedia 清单：中国可访问的历史经典来源 ----------------
   wikidata 那条链路在国内会间歇性被掐，这三张表是备选（en.wikipedia.org 实测可达）：
   - Grammy 年度专辑：1959 至今，一年一张，全年代覆盖
   - 全球最畅销专辑：约百张
   - Billboard 200 年度冠军专辑：逐年页面，每次刷新补几年
   都是「艺人 | 专辑 | 年份」的表，解析出来当历史经典池的原料。 */

fn wiki_stale(file: &PoolFile, key: &str) -> bool {
    file.wiki.get(key).map(|t| now().saturating_sub(*t) > WIKI_TTL_SECS).unwrap_or(true)
}

async fn fetch_wiki_html(client: &reqwest::Client, page: &str) -> Option<String> {
    let url = format!(
        "https://en.wikipedia.org/w/api.php?action=parse&prop=text&format=json&redirects=1&page={}",
        urlencode(page)
    );
    for attempt in 0..3 {
        if attempt > 0 {
            pause(400 * attempt).await;
        }
        let Ok(res) = client.get(&url).send().await else { continue };
        if !res.status().is_success() {
            continue;
        }
        let Ok(json) = res.json::<serde_json::Value>().await else { continue };
        if let Some(html) = json.pointer("/parse/text/*").and_then(|v| v.as_str()) {
            return Some(html.to_string());
        }
    }
    None
}

/// 粗暴地拆表格行：够这几张表用就行
fn html_rows(html: &str) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(p) = rest.find("<tr") {
        let Some(e) = rest[p..].find("</tr>") else { break };
        let cells = html_cells(&rest[p..p + e]);
        if cells.len() >= 2 {
            out.push(cells);
        }
        rest = &rest[p + e + 5..];
    }
    out
}

fn html_cells(row: &str) -> Vec<String> {
    let mut cells = Vec::new();
    let mut rest = row;
    loop {
        let td = rest.find("<td");
        let th = rest.find("<th");
        let (pos, close) = match (td, th) {
            (Some(a), Some(b)) => if a < b { (a, "</td>") } else { (b, "</th>") },
            (Some(a), None) => (a, "</td>"),
            (None, Some(b)) => (b, "</th>"),
            (None, None) => break,
        };
        let Some(open) = rest[pos..].find('>') else { break };
        let start = pos + open + 1;
        let Some(end) = rest[start..].find(close) else { break };
        cells.push(html_text(&rest[start..start + end]));
        rest = &rest[start + end + close.len()..];
    }
    cells
}

fn strip_between(s: &str, open: &str, close: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(p) = rest.find(open) {
        out.push_str(&rest[..p]);
        match rest[p..].find(close) {
            Some(c) => rest = &rest[p + c + close.len()..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

fn html_text(s: &str) -> String {
    let s = strip_between(s, "<ref", "</ref>");
    let s = strip_between(&s, "<!--", "-->");
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    let out = out
        .replace("&amp;", "&")
        .replace("&nbsp;", " ")
        .replace("&#160;", " ")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn clean_entry(s: &str) -> String {
    s.trim()
        .trim_end_matches(['†', '*', ' ', '\u{200b}'])
        .trim_matches('"')
        .trim()
        .to_string()
}

/// 年代清单里的专辑名 vs Apple 返回的专辑名：要么完全一样，
/// 要么是长标题之间的包含关系（"Sgt. Pepper's ... (2017 Mix)" 这种）。
/// 松的包含会被 "The Faith Tour" 蹭到 "Faith"，所以短的标题只认完全相等。
fn title_match(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    short.len() >= 12 && long.contains(short) && long.len() - short.len() <= 6
}

/// Grammy 年度专辑：年份 | 专辑 | 艺人
fn parse_grammy(html: &str) -> Vec<EraEntry> {
    let mut out = Vec::new();
    for cells in html_rows(html) {
        if cells.len() < 3 {
            continue;
        }
        let Some(year) = cells[0].split_whitespace().next().and_then(|s| s.parse::<u16>().ok())
        else {
            continue;
        };
        if !(1950..=2030).contains(&year) {
            continue;
        }
        let title = clean_entry(&cells[1]);
        let artist = clean_entry(&cells[2]);
        if title.is_empty() || artist.is_empty() {
            continue;
        }
        out.push(EraEntry { decade: "grammy".into(), artist, title, year, links: 60 });
    }
    out
}

/// 最畅销专辑：艺人 | 专辑 | 发行年 | 流派 | 销量
fn parse_bestselling(html: &str) -> Vec<EraEntry> {
    let mut out = Vec::new();
    for cells in html_rows(html) {
        if cells.len() < 4 {
            continue;
        }
        let artist = clean_entry(&cells[0]);
        let title = clean_entry(&cells[1]);
        let Some(year) = cells[2].get(..4).and_then(|s| s.parse::<u16>().ok()) else { continue };
        if !(1950..=2030).contains(&year) || artist.is_empty() || title.is_empty() {
            continue;
        }
        out.push(EraEntry { decade: "bestselling".into(), artist, title, year, links: 55 });
    }
    out
}

/// Billboard 200 某年的冠军专辑：日期 | 专辑 | 艺人
fn parse_billboard_year(html: &str, year: u16) -> Vec<EraEntry> {
    const MONTHS: [&str; 12] = [
        "january", "february", "march", "april", "may", "june", "july", "august", "september",
        "october", "november", "december",
    ];
    let mut out = Vec::new();
    for cells in html_rows(html) {
        if cells.len() < 3 {
            continue;
        }
        let first = cells[0].to_lowercase();
        if !MONTHS.iter().any(|m| first.contains(m)) {
            continue;
        }
        let title = clean_entry(&cells[1]);
        let artist = clean_entry(&cells[2]);
        if title.is_empty() || artist.is_empty() {
            continue;
        }
        out.push(EraEntry {
            decade: format!("billboard-{year}"),
            artist,
            title,
            year,
            links: 45,
        });
    }
    out
}

/* ---------------- 用 Apple search 把「经典」配成 Apple 专辑 ---------------- */

fn has_cjk(s: &str) -> bool {
    s.chars().any(|c| matches!(c, '\u{3400}'..='\u{9fff}' | '\u{3040}'..='\u{30ff}'))
}

/// 拿一个歌手的全部专辑（Apple search 返回顺序就是热度顺序）
async fn search_artist_albums(client: &reqwest::Client, artist: &str, sf: &str) -> Vec<PoolAlbum> {
    let url = format!(
        "https://itunes.apple.com/search?term={}&entity=album&limit=200&country={}",
        urlencode(artist),
        sf
    );
    let Ok(res) = client.get(&url).send().await else { return Vec::new() };
    if !res.status().is_success() {
        return Vec::new();
    }
    let Ok(json) = res.json::<serde_json::Value>().await else { return Vec::new() };
    let Some(rows) = json.get("results").and_then(|v| v.as_array()) else { return Vec::new() };
    let want = norm_base(artist);
    let mut out = Vec::new();
    for r in rows {
        let name = r.get("artistName").and_then(|v| v.as_str()).unwrap_or("");
        let na = norm_base(name);
        if !(na == want || na.contains(&want) || want.contains(&na)) {
            continue;
        }
        let id = r.get("collectionId").and_then(|v| v.as_u64()).map(|v| v.to_string()).unwrap_or_default();
        let title = r.get("collectionName").and_then(|v| v.as_str()).unwrap_or("").trim();
        let art = r.get("artworkUrl100").and_then(|v| v.as_str()).and_then(art_base);
        let Some(art) = art else { continue };
        let tracks = r.get("trackCount").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let genre = r.get("primaryGenreName").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if junk(&id, title, name, &genre, &art, tracks) {
            continue;
        }
        out.push(PoolAlbum {
            id,
            sf: sf.to_string(),
            title: title.to_string(),
            artist: name.to_string(),
            date: r.get("releaseDate").and_then(|v| v.as_str()).unwrap_or("").get(..10).unwrap_or("").to_string(),
            genre,
            tracks,
            key: art_key(name, title),
            art,
            artist_id: r.get("artistId").and_then(|v| v.as_u64()).map(|v| v.to_string()).unwrap_or_default(),
            ..Default::default()
        });
    }
    out
}

/* ---------------- 合并进池子 ---------------- */

fn absorb(file: &mut PoolFile, mut rows: Vec<(PoolAlbum, usize)>, src: &str) {
    let mut index: HashMap<String, usize> = HashMap::new();
    for (i, a) in file.albums.iter().enumerate() {
        index.insert(a.id.clone(), i);
    }
    for (mut a, rank) in rows.drain(..) {
        match index.get(&a.id) {
            Some(&i) => {
                let cur = &mut file.albums[i];
                cur.rank = cur.rank.min(rank as u32);
                if cur.artist_id.is_empty() {
                    cur.artist_id = a.artist_id.clone();
                }
                cur.touch(src.to_string());
            }
            None => {
                a.rank = rank as u32;
                a.regions = 1;
                a.touch(src.to_string());
                index.insert(a.id.clone(), file.albums.len());
                file.albums.push(a);
            }
        }
    }
}

/// 统计每张专辑被几个店区收录（跨市场热度）
fn refresh_regions(file: &mut PoolFile) {
    let mut by_key: HashMap<String, HashSet<String>> = HashMap::new();
    for a in &file.albums {
        by_key.entry(a.id.clone()).or_default().insert(a.sf.clone());
    }
    for a in file.albums.iter_mut() {
        if let Some(set) = by_key.get(&a.id) {
            a.regions = set.len() as u32;
        }
    }
}

/// 同一个封面指纹只留一条：优先留**最早**的那版（原始专辑），
/// 而不是"Rumours (Live)"、"Thriller 40" 这种后出版本
fn dedupe_keys(file: &mut PoolFile) {
    let mut best: HashMap<String, usize> = HashMap::new();
    let mut drop: HashSet<usize> = HashSet::new();
    for (i, a) in file.albums.iter().enumerate() {
        if a.key.is_empty() {
            continue;
        }
        match best.get(&a.key) {
            Some(&j) => {
                let cur = &file.albums[j];
                let better = better_edition(a, cur);
                if better {
                    drop.insert(j);
                    best.insert(a.key.clone(), i);
                } else {
                    drop.insert(i);
                }
            }
            None => {
                best.insert(a.key.clone(), i);
            }
        }
    }
    if !drop.is_empty() {
        let mut i = 0usize;
        file.albums.retain(|_| {
            let keep = !drop.contains(&i);
            i += 1;
            keep
        });
    }
}

/// 谁更适合当"这张专辑"：先看年份早（原版），再看知名度，最后看榜上名次
fn better_edition(a: &PoolAlbum, b: &PoolAlbum) -> bool {
    let year = |x: &PoolAlbum| if x.year() == 0 { u16::MAX } else { x.year() };
    if year(a) != year(b) {
        return year(a) < year(b);
    }
    if a.recognition != b.recognition {
        return a.recognition > b.recognition;
    }
    a.rank < b.rank
}

/* ---------------- 刷新 ---------------- */

/// 后台刷一步：Apple 榜单补一批 + 年代榜补几个 + 解析几个歌手的旧作
pub async fn refresh_step(app: AppHandle) {
    let mut file = load_file(&app);
    let Some(client) = http_client(14) else { return };
    /* 池子还小的时候连着跑几轮，第一次打开就能把池子铺开 */
    let rounds = if file.albums.len() < 800 { 3 } else { 1 };
    for _ in 0..rounds {
        if file.albums.len() >= TARGET {
            break;
        }
        let small = file.albums.len() < 800;
        refresh_file(&mut file, &client, small).await;
    }
    file.version = 1;
    file.updated = now();
    let _ = save(&app, &file);
}

/// 刷一轮（抽出来是为了能单独测）
async fn refresh_file(file: &mut PoolFile, client: &reqwest::Client, small: bool) {
    let mut rng = Rng::new();
    /* 1) 当下主流：Apple 榜单 */
    let count = if small { APPLE_BATCH_SMALL } else { APPLE_BATCH_FULL };
    let rows = fetch_apple(client, &mut rng, count).await;
    absorb(file, rows, "apple");

    /* 2) 年代榜：先把缺的年代拉回来（每次最多两个） */
    /* wikidata 这条链路在国内经常连不上，给它一个短超时的客户端，别拖累整轮 */
    let wd = http_client(10).unwrap_or_else(|| client.clone());
    let mut fetched = 0;
    for (decade, from, to) in DECADES {
        if fetched >= ERA_FETCH_BUDGET {
            break;
        }
        let fresh = file.eras.get(decade).map(|t| now().saturating_sub(*t) < ERA_TTL_SECS).unwrap_or(false);
        if fresh {
            continue;
        }
        fetched += 1; /* 不管成没成都只算一次，失败别把 7 个年代全试一遍 */
        let rows = fetch_era(&wd, decade, from, to).await;
        if rows.is_empty() {
            /* 这一路连不上（国内常见），本轮就不再试了，下次刷新再说 */
            break;
        }
        file.era_list.retain(|e| e.decade != decade);
        file.era_list.extend(rows);
        file.eras.insert(decade.to_string(), now());
    }

    /* 2.5) Wikipedia 清单：wikidata 挂了就靠这几张表撑着 */
    let mut budget = if small { 3 } else { WIKI_FETCH_BUDGET };
    if budget > 0 && wiki_stale(file, "grammy") {
        if let Some(html) = fetch_wiki_html(client, "Grammy Award for Album of the Year").await {
            let rows = parse_grammy(&html);
            if !rows.is_empty() {
                file.era_list.retain(|e| e.decade != "grammy");
                file.era_list.extend(rows);
                file.wiki.insert("grammy".into(), now());
                budget -= 1;
            }
        }
    }
    if budget > 0 && wiki_stale(file, "bestselling") {
        if let Some(html) = fetch_wiki_html(client, "List of best-selling albums").await {
            let rows = parse_bestselling(&html);
            if !rows.is_empty() {
                file.era_list.retain(|e| e.decade != "bestselling");
                file.era_list.extend(rows);
                file.wiki.insert("bestselling".into(), now());
                budget -= 1;
            }
        }
    }
    if budget > 0 {
        let this_year = year_of(&iso_today());
        let mut years: Vec<u16> = Vec::new();
        for y in 1970..this_year {
            if years.len() >= budget {
                break;
            }
            if wiki_stale(file, &format!("billboard-{y}")) {
                years.push(y);
            }
        }
        for y in years {
            let page = format!("List of Billboard 200 number-one albums of {y}");
            let Some(html) = fetch_wiki_html(client, &page).await else { continue };
            let rows = parse_billboard_year(&html, y);
            if rows.is_empty() {
                continue;
            }
            file.era_list.retain(|e| e.decade != format!("billboard-{y}"));
            file.era_list.extend(rows);
            file.wiki.insert(format!("billboard-{y}"), now());
        }
    }

    /* 3) 解析名歌手的历史专辑（每次一批，慢慢攒） */
    let budget = if small { ARTIST_BUDGET_SMALL } else { ARTIST_BUDGET_FULL };
    let mut todo: Vec<(String, u32)> = Vec::new();
    {
        let mut seen: HashSet<String> = HashSet::new();
        for e in &file.era_list {
            if file.artists.contains_key(&e.artist) || !seen.insert(e.artist.clone()) {
                continue;
            }
            todo.push((e.artist.clone(), e.links));
        }
    }
    todo.sort_by_key(|e| std::cmp::Reverse(e.1));
    todo.truncate(budget);

    /* 一次解析 6 位歌手（并发），别一位一位串着等 */
    for chunk in todo.chunks(6) {
        let era = &file.era_list;
        let mut jobs = Vec::with_capacity(chunk.len());
        for (artist, _) in chunk {
            let client = client.clone();
            let artist = artist.clone();
            let sf = if has_cjk(&artist) { "tw" } else { "us" };
            let titles: Vec<(String, u32)> = era
                .iter()
                .filter(|e| e.artist == artist)
                .map(|e| (norm_base(&e.title), e.links))
                .collect();
            jobs.push(tauri::async_runtime::spawn(async move {
                let albums = search_artist_albums(&client, &artist, sf).await;
                (artist, titles, albums)
            }));
        }
        for job in jobs {
            let Ok((artist, titles, albums)) = job.await else { continue };
            file.artists.insert(artist, now());
            let mut rows: Vec<(PoolAlbum, usize)> = Vec::new();
            for mut a in albums {
                let nb = norm_base(&a.title);
                /* 只收年代清单里点名的那些专辑，歌手的其他旧作不要（免得全是现场版和冷门） */
                let Some((_, links)) = titles.iter().find(|(t, _)| title_match(t, &nb)) else {
                    continue;
                };
                a.recognition = *links;
                a.classic = true;
                a.src.push("era".into());
                rows.push((a, 99)); /* 99 = 没上过榜，名次很靠后 */
            }
            absorb(file, rows, "era");
        }
    }

    refresh_regions(file);
    rescore(file);
    dedupe_keys(file);
}

/// 需要的时候在后台跑一步（不阻塞界面）
pub fn spawn_refresh_if_needed(app: AppHandle) {
    let file = load_file(&app);
    let empty = file.albums.is_empty();
    let stale = now().saturating_sub(file.updated) > 6 * 3600;
    let below = file.albums.len() < TARGET;
    let topup = now().saturating_sub(file.updated) > TOPUP_SECS;
    if empty || (below && stale) || topup {
        tauri::async_runtime::spawn(async move { refresh_step(app).await });
    }
}

/// 现抓一批榜单（池子还没建起来时的兜底路径用）
pub async fn fetch_fresh(count: usize) -> Vec<(PoolAlbum, usize)> {
    let Some(client) = http_client(10) else { return Vec::new() };
    let mut rng = Rng::new();
    fetch_apple(&client, &mut rng, count).await
}

/// 给界面看的池子概况
#[derive(Serialize)]
pub struct PoolStats {
    pub total: usize,
    pub classic: usize,
    pub updated: u64,
    pub eras: usize,
}

#[tauri::command]
pub fn pool_stats(app: AppHandle) -> PoolStats {
    let file = load_file(&app);
    PoolStats {
        total: file.albums.len(),
        classic: file.albums.iter().filter(|a| a.classic).count(),
        updated: file.updated,
        eras: file.eras.len(),
    }
}
