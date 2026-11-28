//! Telegram bot front-end for the MovieBox-TUI provider library.
//!
//! Flow: search all providers -> one combined result list -> pick title -> (season/episode)
//! -> All audio languages -> pick quality/source (all providers merged) -> play / download.
//!
//! Runs with long polling (no server needed), so it works inside GitHub Actions.
//!
//! Env:
//!   BOT_TOKEN          Telegram bot token (required)
//!   ALLOWED_CHAT_IDS   Optional comma-separated chat ids allowed to use the bot
//!   RUN_MINUTES        Optional, exit gracefully after N minutes (default 350)

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use moviebox_tui::providers::ReleaseProvider;
use moviebox_tui::providers::models::{
    CatalogItem, Episode, MediaDetails, MediaType, ProviderError, ProviderKind, Release,
    ResolutionIntent, Season,
};
use moviebox_tui::service::MovieBoxService;
use serde_json::{Value, json};

/// How many ads must be watched to unlock Play / Download.
const ADS_REQUIRED: usize = 1;

/// "1 ad" / "2 ads"
fn ads_word(n: usize) -> String {
    if n == 1 {
        "1 ad".to_string()
    } else {
        format!("{n} ads")
    }
}

const PAGE_ITEMS: usize = 10;
const PAGE_EPISODES: usize = 30;
const MAX_QUALITY_BUTTONS: usize = 90;

// ---------------------------------------------------------------- Telegram

#[derive(Clone)]
struct Tg {
    http: reqwest::Client,
    base: String,
}

impl Tg {
    fn new(token: &str) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .build()
            .unwrap_or_default();
        Self {
            http,
            base: format!("https://api.telegram.org/bot{token}"),
        }
    }

    async fn call(&self, method: &str, body: &Value) -> Result<Value, String> {
        let resp = self
            .http
            .post(format!("{}/{}", self.base, method))
            .timeout(Duration::from_secs(70))
            .json(body)
            .send()
            .await
            .map_err(|e| e.without_url().to_string())?;
        let v: Value = resp.json().await.map_err(|e| e.without_url().to_string())?;
        if v.get("ok").and_then(Value::as_bool) == Some(true) {
            Ok(v.get("result").cloned().unwrap_or(Value::Null))
        } else {
            Err(v
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("telegram error")
                .to_string())
        }
    }

    async fn send(&self, chat: i64, text: &str, kb: Option<Value>) -> Result<i64, String> {
        let mut body = json!({
            "chat_id": chat,
            "text": text,
            "parse_mode": "HTML",
            "link_preview_options": {"is_disabled": true}
        });
        if let Some(k) = kb {
            body["reply_markup"] = k;
        }
        let r = self.call("sendMessage", &body).await?;
        Ok(r.get("message_id").and_then(Value::as_i64).unwrap_or(0))
    }

    async fn edit(&self, chat: i64, msg: i64, text: &str, kb: Option<Value>) -> Result<(), String> {
        let mut body = json!({
            "chat_id": chat,
            "message_id": msg,
            "text": text,
            "parse_mode": "HTML",
            "link_preview_options": {"is_disabled": true}
        });
        if let Some(k) = kb {
            body["reply_markup"] = k;
        }
        self.call("editMessageText", &body).await.map(|_| ())
    }

    async fn photo(&self, chat: i64, url: &str, caption: &str) -> Result<(), String> {
        let body = json!({
            "chat_id": chat,
            "photo": url,
            "caption": caption,
            "parse_mode": "HTML"
        });
        self.call("sendPhoto", &body).await.map(|_| ())
    }

    async fn answer(&self, callback_id: &str, text: &str) {
        let _ = self
            .call(
                "answerCallbackQuery",
                &json!({"callback_query_id": callback_id, "text": text}),
            )
            .await;
    }
}

// ------------------------------------------------------------------ State

#[derive(Default, Clone)]
struct ChatState {
    query: String,
    /// Providers that did not answer the last search (shown under the results)
    note: String,
    /// What each provider returned for the chosen title (shown under the links)
    streams_note: String,
    /// Titles merged from all providers (same name + release year)
    groups: Vec<Group>,
    /// Details of the chosen title from every provider that has it
    members: Vec<(ProviderKind, MediaDetails)>,
    /// Providers whose details failed to load: their stream ids are still used
    fallback: Vec<(ProviderKind, String)>,
    /// Titles that are one season of a series
    declared: Vec<Declared>,
    /// Details used for the card and the season / episode lists
    primary: Option<MediaDetails>,
    /// "Name (Year)" of the chosen title
    title: String,
    season: usize,
    episode: usize,
    targets: Vec<Target>,
    releases: Vec<Release>,
    /// For every release: index into `targets`
    rel_target: Vec<usize>,
    /// Releases whose links turned out to be dead (hidden from the list)
    dead: Vec<usize>,
}

/// One provider subject the streams were fetched from.
#[derive(Clone)]
struct Target {
    provider: ProviderKind,
    subject_id: String,
    siblings: Vec<String>,
    /// Per-season titles use their own season number (usually 1)
    season_override: Option<usize>,
    /// Audio language of this source (MovieBox dubs)
    audio: Option<String>,
}

/// A provider title that is exactly one season of a series ("Show S2").
#[derive(Clone)]
struct Declared {
    provider: ProviderKind,
    subject_id: String,
    /// the season it stands for (2 in "Show S2")
    season: usize,
    /// the season number to send to the provider for this title
    own_season: usize,
}

#[derive(Clone)]
struct Ctx {
    tg: Tg,
    svc: MovieBoxService,
    states: Arc<Mutex<HashMap<i64, ChatState>>>,
    allowed: Arc<Vec<i64>>,
    /// Public https base of the built-in stream server (cloudflared quick tunnel)
    base: Arc<Mutex<Option<String>>>,
    streams: StreamMap,
}

fn st_get(ctx: &Ctx, chat: i64) -> ChatState {
    ctx.states
        .lock()
        .map(|g| g.get(&chat).cloned().unwrap_or_default())
        .unwrap_or_default()
}

fn st_set<F: FnOnce(&mut ChatState)>(ctx: &Ctx, chat: i64, f: F) {
    if let Ok(mut g) = ctx.states.lock() {
        f(g.entry(chat).or_default());
    }
}

// ---------------------------------------------------------------- Helpers

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn short(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let t: String = s.chars().take(n.saturating_sub(1)).collect();
        format!("{t}…")
    }
}

fn fmt_size(bytes: u64) -> String {
    let b = bytes as f64;
    if b >= 1_073_741_824.0 {
        format!("{:.2} GB", b / 1_073_741_824.0)
    } else {
        format!("{:.0} MB", b / 1_048_576.0)
    }
}

fn cb(text: &str, data: String) -> Value {
    json!({"text": text, "callback_data": data})
}

fn kb(rows: Vec<Vec<Value>>) -> Value {
    json!({"inline_keyboard": rows})
}

/// Which provider / addon a link comes from ("MovieBox", "HdHub", "Torrentio", ...).
fn provider_name(r: &Release) -> String {
    match r.provider {
        ProviderKind::Addons => r
            .source_label()
            .split(" · ")
            .next()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .unwrap_or("Addons")
            .to_string(),
        p => p.label().to_string(),
    }
}

/// Extra source text (uploader / stream title), when it adds something.
fn source_detail(r: &Release) -> Option<String> {
    let src = r.source_label().trim();
    let detail = match r.provider {
        ProviderKind::Addons => src.split_once(" · ").map(|(_, b)| b.trim()).unwrap_or(""),
        _ => src,
    };
    if detail.is_empty()
        || detail.eq_ignore_ascii_case("direct")
        || detail.eq_ignore_ascii_case(&provider_name(r))
    {
        None
    } else {
        Some(detail.to_string())
    }
}

fn language_of(r: &Release) -> Option<String> {
    r.language
        .as_ref()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
}

/// Short text for a button: "1080p • 3.23 GB • Hindi • MovieBox".
fn rel_short(r: &Release) -> String {
    let mut parts: Vec<String> = vec![r.quality.clone().unwrap_or_else(|| "Auto".to_string())];
    if let Some(sz) = r.size_bytes {
        parts.push(fmt_size(sz));
    }
    if let Some(l) = language_of(r) {
        parts.push(short(&l, 14));
    }
    parts.push(short(&provider_name(r), 14));
    parts.join(" • ")
}

/// Full text for the message: quality, size, audio language, codec, provider.
fn rel_full(r: &Release) -> String {
    let mut parts: Vec<String> = vec![r.quality.clone().unwrap_or_else(|| "Auto".to_string())];
    if let Some(sz) = r.size_bytes {
        parts.push(fmt_size(sz));
    }
    if let Some(l) = language_of(r) {
        parts.push(l);
    }
    if let Some(c) = r.codec.as_ref().filter(|c| !c.trim().is_empty()) {
        parts.push(c.trim().to_string());
    }
    let mut source = provider_name(r);
    if let Some(d) = source_detail(r) {
        source.push_str(&format!(" · {d}"));
    }
    parts.push(source);
    parts.join(" • ")
}

fn episode_tag(st: &ChatState) -> String {
    if st.season > 0 {
        format!(" S{:02}E{:02}", st.season, st.episode)
    } else {
        String::new()
    }
}

async fn show(ctx: &Ctx, chat: i64, msg: Option<i64>, text: &str, markup: Option<Value>) {
    match msg {
        Some(m) => {
            if let Err(e) = ctx.tg.edit(chat, m, text, markup.clone()).await {
                if !e.contains("not modified") {
                    let _ = ctx.tg.send(chat, text, markup).await;
                }
            }
        }
        None => {
            let _ = ctx.tg.send(chat, text, markup).await;
        }
    }
}

const HELP: &str = "<b>MahSurat Stream Bot</b>\n\n\
Send a movie or series name (or use /search name).\n\
I search <b>all providers</b> at once and show one combined list:\n\
1. Pick the title (name + year)\n2. Season / Episode (series)\n3. All audio languages\n4. Pick a quality / source (links from every provider)\n\n\
Then play it in Telegram, MX Player or VLC, or tap ⬇️ Download.\n\n\
/search &lt;name&gt; - search all providers\n/id - show your chat id";

// ----------------------------------------------------------------- Search

/// Providers the bot searches (the TUI list plus built-in Stremio addons).
const BOT_PROVIDERS: [ProviderKind; 6] = [
    ProviderKind::MovieBox,
    ProviderKind::FourKHdHub,
    ProviderKind::Dramachi,
    ProviderKind::BdixCircleFtp,
    ProviderKind::BdixDhakaFlix,
    ProviderKind::Addons,
];

/// Order used to pick the provider whose metadata (poster, description,
/// seasons) is shown on the title card.
const DETAIL_PRIORITY: [ProviderKind; 6] = [
    ProviderKind::Addons,
    ProviderKind::MovieBox,
    ProviderKind::FourKHdHub,
    ProviderKind::Dramachi,
    ProviderKind::BdixCircleFtp,
    ProviderKind::BdixDhakaFlix,
];

/// One title (same name + release year) found on one or more providers.
#[derive(Clone)]
struct Group {
    title: String,
    year: String,
    series: bool,
    poster: Option<String>,
    members: Vec<(ProviderKind, CatalogItem)>,
}

/// Lower-case title without bracketed text, punctuation or a leading "the".
fn norm_title(s: &str) -> String {
    let (base, _) = split_season(s);
    let s = base.as_str();
    let mut out = String::new();
    let mut depth = 0u32;
    for c in s.chars() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            _ if depth > 0 => {}
            '&' => out.push_str(" and "),
            c if c.is_alphanumeric() => out.extend(c.to_lowercase()),
            _ => out.push(' '),
        }
    }
    let joined = out.split_whitespace().collect::<Vec<_>>().join(" ");
    match joined.strip_prefix("the ") {
        Some(rest) => rest.to_string(),
        None => joined,
    }
}

/// "Bachelor Point S2" / "Bachelor Point Season 2" -> ("Bachelor Point", Some(2)).
/// MovieBox lists every season of some series as its own title.
fn split_season(raw: &str) -> (String, Option<usize>) {
    let cleaned = clean_title(raw);
    let mut words: Vec<&str> = cleaned.split_whitespace().collect();
    let mut season: Option<usize> = None;
    if words.len() >= 2 {
        let last = words[words.len() - 1];
        let digits =
            |d: &str| !d.is_empty() && d.len() <= 2 && d.chars().all(|c| c.is_ascii_digit());
        if let Some(d) = last.strip_prefix(['s', 'S']).filter(|d| digits(d)) {
            season = d.parse().ok().filter(|n| *n > 0);
            if season.is_some() {
                words.pop();
            }
        } else if words.len() >= 3
            && words[words.len() - 2].eq_ignore_ascii_case("season")
            && digits(last)
        {
            season = last.parse().ok().filter(|n| *n > 0);
            if season.is_some() {
                words.pop();
                words.pop();
            }
        }
    }
    (words.join(" "), season)
}

/// Title for display: original case, bracketed text removed.
fn clean_title(s: &str) -> String {
    let mut out = String::new();
    let mut depth = 0u32;
    for c in s.chars() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            _ if depth > 0 => {}
            _ => out.push(c),
        }
    }
    let t = out.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.is_empty() {
        s.trim().to_string()
    } else {
        t
    }
}

/// "Avengers Endgame 2019" with release year 2019 -> "Avengers Endgame".
fn strip_trailing_year(title: &str, year: &str) -> String {
    let t = title.trim();
    if year.len() == 4 {
        if let Some(rest) = t.strip_suffix(year) {
            let base = rest.trim_end();
            if rest.ends_with(char::is_whitespace) && !base.is_empty() {
                return base.to_string();
            }
        }
    }
    t.to_string()
}

/// First 4-digit year (19xx / 20xx) found in the text, or "".
fn year_of(y: Option<&str>) -> String {
    let Some(y) = y else {
        return String::new();
    };
    let chars: Vec<char> = y.chars().collect();
    for w in chars.windows(4) {
        let digits = w.iter().all(|c| c.is_ascii_digit());
        let century = (w[0] == '1' && w[1] == '9') || (w[0] == '2' && w[1] == '0');
        if digits && century {
            return w.iter().collect();
        }
    }
    String::new()
}

/// Release years of the same title often differ by one between providers.
fn years_close(a: &str, b: &str) -> bool {
    match (a.parse::<i32>(), b.parse::<i32>()) {
        (Ok(x), Ok(y)) => (x - y).abs() <= 1,
        _ => false,
    }
}

fn distinct_providers(members: &[(ProviderKind, CatalogItem)]) -> usize {
    let mut seen: Vec<ProviderKind> = Vec::new();
    for (p, _) in members {
        if !seen.contains(p) {
            seen.push(*p);
        }
    }
    seen.len()
}

/// Merge the results of all providers: same name + same release year = one title.
fn build_groups(results: &[(ProviderKind, Vec<CatalogItem>)]) -> Vec<Group> {
    let mut groups: Vec<Group> = Vec::new();
    let mut keys: Vec<(String, String)> = Vec::new();
    // Groups that contain per-season titles ("Show S1", "Show S2", ...): their
    // members have different release years, so the year is not compared.
    let mut suffixed: Vec<bool> = Vec::new();
    // First the items that have a year, then the ones without (they join any
    // title with the same name).
    for with_year in [true, false] {
        for (p, items) in results {
            for it in items {
                let y = year_of(it.year.as_deref());
                let tt = strip_trailing_year(&it.title, &y);
                let nt = norm_title(&tt);
                if nt.is_empty() || y.is_empty() == with_year {
                    continue;
                }
                let series = it.media_type == MediaType::Series;
                let poster = it.poster_url.clone().filter(|u| u.starts_with("http"));
                let has_suffix = split_season(&tt).1.is_some();
                let pos = keys.iter().enumerate().position(|(i, (kt, ky))| {
                    *kt == nt && (y.is_empty() || has_suffix || suffixed[i] || years_close(ky, &y))
                });
                match pos {
                    Some(i) => {
                        suffixed[i] |= has_suffix;
                        if has_suffix && !y.is_empty() && (keys[i].1.is_empty() || y < keys[i].1) {
                            keys[i].1 = y.clone();
                            groups[i].year = y.clone();
                        }
                        let g = &mut groups[i];
                        let dup = g
                            .members
                            .iter()
                            .any(|(mp, mi)| mp == p && mi.id.value == it.id.value);
                        if g.members.len() < 12 && !dup {
                            g.members.push((*p, it.clone()));
                        }
                        g.series |= series;
                        if g.poster.is_none() {
                            g.poster = poster;
                        }
                    }
                    None => {
                        keys.push((nt, y.clone()));
                        suffixed.push(has_suffix);
                        groups.push(Group {
                            title: split_season(&tt).0,
                            year: y,
                            series,
                            poster,
                            members: vec![(*p, it.clone())],
                        });
                    }
                }
            }
        }
    }
    // Newest first, oldest last (no year = last). Same year: the title found
    // on more providers first.
    groups.sort_by(|a, b| {
        let ya: i32 = a.year.parse().unwrap_or(0);
        let yb: i32 = b.year.parse().unwrap_or(0);
        yb.cmp(&ya)
            .then_with(|| distinct_providers(&b.members).cmp(&distinct_providers(&a.members)))
    });
    groups
}

async fn do_search(ctx: &Ctx, chat: i64, query: &str) {
    let wait = ctx
        .tg
        .send(
            chat,
            &format!("🔎 Searching all providers for <b>{}</b>…", esc(query)),
            None,
        )
        .await
        .ok();

    let futs = BOT_PROVIDERS.into_iter().map(|p| {
        let svc = ctx.svc.clone();
        let q = query.to_string();
        async move {
            let r = tokio::time::timeout(Duration::from_secs(25), svc.search_typed(p, &q, 1)).await;
            (p, r)
        }
    });
    let outs = futures::future::join_all(futs).await;

    let mut results: Vec<(ProviderKind, Vec<CatalogItem>)> = Vec::new();
    let mut failed: Vec<&'static str> = Vec::new();
    for (p, r) in outs {
        match r {
            Ok(Ok(items)) => {
                eprintln!("[search] {} -> {} results", p.label(), items.len());
                if !items.is_empty() {
                    results.push((p, items));
                }
            }
            Ok(Err(ProviderError::NotFound)) => {
                eprintln!("[search] {} -> no results", p.label());
            }
            Ok(Err(e)) => {
                eprintln!("[search] {} failed: {e:?}", p.label());
                if !p.is_bdix() {
                    failed.push(p.label());
                }
            }
            Err(_) => {
                eprintln!("[search] {} timed out", p.label());
                if !p.is_bdix() {
                    failed.push(p.label());
                }
            }
        }
    }
    let groups = build_groups(&results);
    let q = query.to_string();
    let note = if failed.is_empty() {
        String::new()
    } else {
        format!("⚠️ Not responding: {}", failed.join(", "))
    };
    st_set(ctx, chat, |s| {
        *s = ChatState::default();
        s.query = q;
        s.groups = groups;
        s.note = note;
    });
    show_results(ctx, chat, wait, 0).await;
}

/// One combined list ("Total results") with every title from every provider.
async fn show_results(ctx: &Ctx, chat: i64, msg: Option<i64>, page: usize) {
    let st = st_get(ctx, chat);
    let total = st.groups.len();
    if total == 0 {
        let mut text = format!(
            "No results for <b>{}</b>. Try another name.",
            esc(&st.query)
        );
        if !st.note.is_empty() {
            text.push_str(&format!("\n\n{}", esc(&st.note)));
        }
        show(ctx, chat, msg, &text, None).await;
        return;
    }
    let start = (page * PAGE_ITEMS).min(total);
    let end = (start + PAGE_ITEMS).min(total);
    let mut rows: Vec<Vec<Value>> = Vec::new();
    for (gi, g) in st.groups.iter().enumerate().take(end).skip(start) {
        let icon = if g.series { "📺" } else { "🎬" };
        let year = if g.year.is_empty() {
            String::new()
        } else {
            format!(" ({})", g.year)
        };
        rows.push(vec![cb(
            &short(&format!("{icon} {}{year}", g.title), 56),
            format!("d:{gi}"),
        )]);
    }
    let mut nav = Vec::new();
    if page > 0 {
        nav.push(cb("◀ Prev", format!("g:{}", page - 1)));
    }
    if end < total {
        nav.push(cb("Next ▶", format!("g:{}", page + 1)));
    }
    if !nav.is_empty() {
        rows.push(nav);
    }
    let mut text = format!(
        "Results for <b>{}</b>\n\n✅ Total results: {}\nShowing {}-{}\n\nPick a title:",
        esc(&st.query),
        total,
        start + 1,
        end
    );
    if !st.note.is_empty() {
        text.push_str(&format!("\n\n{}", esc(&st.note)));
    }
    show(ctx, chat, msg, &text, Some(kb(rows))).await;
}

async fn expired(ctx: &Ctx, chat: i64) {
    let _ = ctx
        .tg
        .send(chat, "Session expired. Please search again.", None)
        .await;
}

// ---------------------------------------------------------------- Details

fn card(d: &MediaDetails, found_on: &str) -> String {
    let mut s = format!("<b>{}</b>", esc(&d.title));
    if let Some(y) = d.year.as_ref().filter(|y| !y.is_empty()) {
        s.push_str(&format!(" ({})", esc(y)));
    }
    s.push_str(&format!(
        "\n{} • On: {}",
        if d.is_series() { "Series" } else { "Movie" },
        esc(found_on)
    ));
    if let Some(r) = d.imdb_rating.as_ref().filter(|r| !r.is_empty()) {
        s.push_str(&format!(" • ⭐ {}", esc(r)));
    }
    if let Some(r) = d.duration.as_ref().filter(|r| !r.is_empty()) {
        s.push_str(&format!(" • {}", esc(r)));
    }
    if !d.genres.is_empty() {
        s.push_str(&format!("\n{}", esc(&d.genres.join(", "))));
    }
    if let Some(a) = d.stars.as_ref().filter(|a| !a.is_empty()) {
        s.push_str(&format!("\n👥 {}", esc(&short(a, 120))));
    }
    if let Some(desc) = d.description.as_ref().filter(|a| !a.is_empty()) {
        let used = s.chars().count();
        let budget = 940usize.saturating_sub(used);
        if budget > 40 {
            s.push_str(&format!("\n\n{}", esc(&short(desc, budget))));
        }
    }
    s
}

/// A title was picked: load its details from every provider that has it.
async fn open_group(ctx: &Ctx, chat: i64, gi: usize) {
    let st = st_get(ctx, chat);
    let Some(mut group) = st.groups.get(gi).cloned() else {
        expired(ctx, chat).await;
        return;
    };
    let wait = ctx
        .tg
        .send(chat, "⏳ Loading details from all providers…", None)
        .await
        .ok();

    // Look the title up again, by its exact name, on every provider that does
    // not have it yet: a provider's top results for the typed words can miss it.
    let gnt = norm_title(&group.title);
    let missing: Vec<ProviderKind> = BOT_PROVIDERS
        .iter()
        .copied()
        .filter(|p| !group.members.iter().any(|(mp, _)| mp == p))
        .collect();
    let lookups = missing.into_iter().map(|p| {
        let svc = ctx.svc.clone();
        let title = group.title.clone();
        async move {
            let r =
                tokio::time::timeout(Duration::from_secs(12), svc.search_typed(p, &title, 1)).await;
            (p, r)
        }
    });
    let found = futures::future::join_all(lookups).await;
    for (p, r) in found {
        let Ok(Ok(items)) = r else {
            continue;
        };
        let mut added = 0;
        for it in items {
            let y = year_of(it.year.as_deref());
            let same_year = y.is_empty() || group.year.is_empty() || years_close(&group.year, &y);
            let same_kind = (it.media_type == MediaType::Series) == group.series;
            let dup = group
                .members
                .iter()
                .any(|(mp, mi)| *mp == p && mi.id.value == it.id.value);
            if added < 2
                && group.members.len() < 12
                && same_kind
                && same_year
                && !dup
                && norm_title(&strip_trailing_year(&it.title, &y)) == gnt
            {
                group.members.push((p, it));
                added += 1;
            }
        }
    }

    let declared_by_id: std::collections::HashMap<String, usize> = group
        .members
        .iter()
        .filter_map(|(_, it)| split_season(&it.title).1.map(|n| (it.id.value.clone(), n)))
        .collect();
    let futs = group.members.iter().map(|(p, item)| {
        let svc = ctx.svc.clone();
        let p = *p;
        let id = item.id.value.clone();
        async move {
            let r = tokio::time::timeout(Duration::from_secs(30), svc.details_typed(p, &id)).await;
            (p, id, r)
        }
    });
    let outs = futures::future::join_all(futs).await;

    let mut members: Vec<(ProviderKind, MediaDetails)> = Vec::new();
    let mut declared: Vec<Declared> = Vec::new();
    let mut fallback: Vec<(ProviderKind, String)> = Vec::new();
    let mut first_err: Option<String> = None;
    for (p, id, r) in outs {
        match r {
            Ok(Ok(d)) => {
                if let Some(n) = declared_by_id.get(&id) {
                    let own = if d.seasons.len() == 1 {
                        d.seasons[0].number
                    } else if d.seasons.iter().any(|s| s.number == *n) {
                        *n
                    } else {
                        1
                    };
                    declared.push(Declared {
                        provider: p,
                        subject_id: d.id.value.clone(),
                        season: *n,
                        own_season: own,
                    });
                }
                members.push((p, d));
            }
            Ok(Err(e)) => {
                eprintln!("[details] {} failed: {e:?}", p.label());
                fallback.push((p, id));
                if first_err.is_none() {
                    first_err = Some(e.user_message(p));
                }
            }
            Err(_) => {
                eprintln!("[details] {} timed out", p.label());
                fallback.push((p, id));
                if first_err.is_none() {
                    first_err = Some(format!("{} timed out loading details.", p.label()));
                }
            }
        }
    }
    if members.is_empty() {
        let msg = first_err.unwrap_or_else(|| "Could not load details.".to_string());
        show(ctx, chat, wait, &format!("⚠️ {}", esc(&msg)), None).await;
        return;
    }
    members.sort_by_key(|(p, _)| {
        DETAIL_PRIORITY
            .iter()
            .position(|x| x == p)
            .unwrap_or(usize::MAX)
    });

    // Metadata / seasons come from the best provider that has them.
    let with_seasons = members.iter().find(|(_, d)| !d.seasons.is_empty());
    let chosen = if group.series {
        with_seasons.or(members.first())
    } else {
        members.first()
    };
    let Some(mut primary) = chosen.map(|(_, d)| d.clone()) else {
        expired(ctx, chat).await;
        return;
    };

    // MovieBox lists some series as one title per season. Show all those
    // seasons in the season list (Season 1, 2, 3, ...).
    if group.series && !declared.is_empty() {
        let mut ordered = declared.clone();
        ordered.sort_by_key(|d| d.season);
        let mut merged: Vec<Season> = Vec::new();
        for dd in &ordered {
            if merged.iter().any(|s| s.number == dd.season) {
                continue;
            }
            let Some((_, det)) = members
                .iter()
                .find(|(mp, md)| *mp == dd.provider && md.id.value == dd.subject_id)
            else {
                continue;
            };
            let eps: Vec<Episode> = det
                .seasons
                .iter()
                .find(|s| s.number == dd.own_season)
                .or(det.seasons.first())
                .map(|s| {
                    s.episodes
                        .iter()
                        .map(|e| Episode {
                            season: dd.season,
                            ..e.clone()
                        })
                        .collect()
                })
                .unwrap_or_default();
            if !eps.is_empty() {
                merged.push(Season {
                    number: dd.season,
                    episodes: eps,
                });
            }
        }
        let other = members
            .iter()
            .filter(|(p, d)| {
                !declared
                    .iter()
                    .any(|x| x.provider == *p && x.subject_id == d.id.value)
            })
            .map(|(_, d)| d)
            .find(|d| !d.seasons.is_empty());
        let other_count = other.map_or(0, |d| d.seasons.len());
        if merged.len() > other_count {
            primary.seasons = merged;
        } else if let Some(o) = other {
            primary.seasons = o.seasons.clone();
        }
    }

    let mut names: Vec<&str> = Vec::new();
    for (p, _) in &group.members {
        if !names.contains(&p.label()) {
            names.push(p.label());
        }
    }
    let caption = card(&primary, &names.join(", "));
    let ok_url = |u: &String| u.starts_with("http");
    let poster = primary
        .poster_url
        .clone()
        .filter(ok_url)
        .or_else(|| {
            members
                .iter()
                .find_map(|(_, d)| d.poster_url.clone().filter(ok_url))
        })
        .or_else(|| group.poster.clone());

    let year = if group.year.is_empty() {
        year_of(primary.year.as_deref())
    } else {
        group.year.clone()
    };
    let title = if year.is_empty() {
        group.title.clone()
    } else {
        format!("{} ({})", group.title, year)
    };
    let p2 = primary.clone();
    st_set(ctx, chat, |s| {
        s.members = members;
        s.declared = declared;
        s.fallback = fallback;
        s.primary = Some(p2);
        s.title = title;
        s.season = 0;
        s.episode = 0;
        s.targets.clear();
        s.releases.clear();
        s.rel_target.clear();
        s.dead.clear();
    });

    let mut sent_photo = false;
    if let Some(url) = poster {
        sent_photo = ctx.tg.photo(chat, &url, &caption).await.is_ok();
    }
    if !sent_photo {
        let _ = ctx.tg.send(chat, &caption, None).await;
    }
    if let Some(w) = wait {
        let _ = ctx
            .tg
            .call("deleteMessage", &json!({"chat_id": chat, "message_id": w}))
            .await;
    }

    if primary.is_series() && !primary.seasons.is_empty() {
        show_seasons(ctx, chat, None).await;
    } else {
        show_audio(ctx, chat, None).await;
    }
}

async fn show_seasons(ctx: &Ctx, chat: i64, msg: Option<i64>) {
    let st = st_get(ctx, chat);
    let Some(d) = st.primary.as_ref() else {
        expired(ctx, chat).await;
        return;
    };
    let mut rows: Vec<Vec<Value>> = Vec::new();
    let mut row: Vec<Value> = Vec::new();
    for s in d.seasons.iter().take(40) {
        row.push(cb(
            &format!("Season {}", s.number),
            format!("s:{}:0", s.number),
        ));
        if row.len() == 4 {
            rows.push(std::mem::take(&mut row));
        }
    }
    if !row.is_empty() {
        rows.push(row);
    }
    show(ctx, chat, msg, "Choose a season:", Some(kb(rows))).await;
}

async fn show_episodes(ctx: &Ctx, chat: i64, msg: Option<i64>, season: usize, page: usize) {
    let st = st_get(ctx, chat);
    let Some(d) = st.primary.as_ref() else {
        expired(ctx, chat).await;
        return;
    };
    let Some(se) = d.seasons.iter().find(|s| s.number == season) else {
        expired(ctx, chat).await;
        return;
    };
    let total = se.episodes.len();
    let start = (page * PAGE_EPISODES).min(total);
    let end = (start + PAGE_EPISODES).min(total);
    let mut rows: Vec<Vec<Value>> = Vec::new();
    let mut row: Vec<Value> = Vec::new();
    for ep in se.episodes.iter().take(end).skip(start) {
        row.push(cb(
            &format!("E{}", ep.number),
            format!("e:{}:{}", season, ep.number),
        ));
        if row.len() == 5 {
            rows.push(std::mem::take(&mut row));
        }
    }
    if !row.is_empty() {
        rows.push(row);
    }
    let mut nav = Vec::new();
    if page > 0 {
        nav.push(cb("◀ Prev", format!("s:{season}:{}", page - 1)));
    }
    if end < total {
        nav.push(cb("Next ▶", format!("s:{season}:{}", page + 1)));
    }
    if !nav.is_empty() {
        rows.push(nav);
    }
    rows.push(vec![cb("⬅ Seasons", "ss".to_string())]);
    let text = format!("<b>Season {season}</b> — choose an episode ({total} total):");
    show(ctx, chat, msg, &text, Some(kb(rows))).await;
}

// ------------------------------------------------- Audio / streams / quality

/// The only audio choice: everything from every provider.
async fn show_audio(ctx: &Ctx, chat: i64, msg: Option<i64>) {
    let rows = vec![vec![cb("🔊 All audio languages", "al".to_string())]];
    show(ctx, chat, msg, "Choose the audio language:", Some(kb(rows))).await;
}

async fn provider_streams(
    svc: &MovieBoxService,
    p: ProviderKind,
    id: &str,
    season: usize,
    episode: usize,
) -> Result<Vec<Release>, ProviderError> {
    match p {
        ProviderKind::MovieBox => {
            ReleaseProvider::episode_streams(&svc.client, id, season, episode).await
        }
        ProviderKind::FourKHdHub => match svc.fourk_client.as_ref() {
            Some(c) => ReleaseProvider::episode_streams(c, id, season, episode).await,
            None => Err(ProviderError::Unavailable(
                "4KHDHub is unavailable".to_string(),
            )),
        },
        ProviderKind::Dramachi => {
            ReleaseProvider::episode_streams(&svc.dramachi_client, id, season, episode).await
        }
        ProviderKind::BdixCircleFtp => {
            ReleaseProvider::episode_streams(&svc.circleftp_client, id, season, episode).await
        }
        ProviderKind::BdixDhakaFlix => {
            ReleaseProvider::episode_streams(&svc.dhakaflix_client, id, season, episode).await
        }
        ProviderKind::Addons => {
            ReleaseProvider::episode_streams(&svc.addon_client, id, season, episode).await
        }
    }
}

/// Fetch the streams of this title from every provider at the same time and
/// merge them into one list.
/// What one provider returned for the chosen title.
#[derive(Default)]
struct Summary {
    found: usize,
    hidden: usize,
    why: Option<String>,
    hidden_why: Option<String>,
}

fn summary_mut(list: &mut [(ProviderKind, Summary)], p: ProviderKind) -> Option<&mut Summary> {
    list.iter_mut().find(|(q, _)| *q == p).map(|(_, s)| s)
}

/// "Sources: MovieBox 14 • 4KHDHub 0 (no streams) • Addons 3 (2 hidden)".
fn sources_note(
    summary: &[(ProviderKind, Summary)],
    shown_of: &dyn Fn(ProviderKind) -> usize,
    absent: &[&str],
) -> String {
    let mut parts: Vec<String> = Vec::new();
    for (p, sm) in summary {
        let shown = shown_of(*p);
        let mut part = format!("{} {}", p.label(), shown);
        if shown == 0 {
            let why = if sm.hidden > 0 {
                format!(
                    "{} link(s) hidden: {}",
                    sm.hidden,
                    sm.hidden_why.clone().unwrap_or_default()
                )
            } else {
                sm.why.clone().unwrap_or_else(|| "no streams".to_string())
            };
            part.push_str(&format!(" ({})", short(&why, 70)));
        } else if sm.hidden > 0 {
            part.push_str(&format!(" ({} hidden)", sm.hidden));
        }
        parts.push(part);
    }
    let mut note = format!("ℹ️ Sources: {}", parts.join(" • "));
    if !absent.is_empty() {
        note.push_str(&format!("\nNot found on: {}", absent.join(", ")));
    }
    note
}

async fn fetch_all(ctx: &Ctx, chat: i64, msg: Option<i64>) {
    let st = st_get(ctx, chat);
    let target_msg = match msg {
        Some(m) => {
            let _ = ctx
                .tg
                .edit(
                    chat,
                    m,
                    "⏳ Fetching streams from all providers and checking the links…",
                    None,
                )
                .await;
            Some(m)
        }
        None => ctx
            .tg
            .send(
                chat,
                "⏳ Fetching streams from all providers and checking the links…",
                None,
            )
            .await
            .ok(),
    };

    // One target per provider subject (MovieBox has one subject per audio dub).
    let mut targets: Vec<Target> = Vec::new();
    for (p, d) in &st.members {
        // A per-season title only answers for its own season.
        let decl = st
            .declared
            .iter()
            .find(|x| x.provider == *p && x.subject_id == d.id.value);
        let season_override = match decl {
            Some(x) if x.season != st.season => continue,
            Some(x) => Some(x.own_season),
            None => None,
        };
        let mut ids: Vec<String> = Vec::new();
        if *p == ProviderKind::MovieBox {
            ids.extend(d.dubs.iter().map(|x| x.subject_id.clone()));
        }
        ids.push(d.id.value.clone());
        let siblings = d.sibling_ids();
        for id in ids {
            if id.is_empty()
                || targets
                    .iter()
                    .any(|t| t.provider == *p && t.subject_id == id)
            {
                continue;
            }
            let audio = d
                .dubs
                .iter()
                .find(|x| x.subject_id == id)
                .map(|x| x.language.trim().to_string())
                .filter(|l| !l.is_empty() && !l.eq_ignore_ascii_case("unknown"));
            targets.push(Target {
                provider: *p,
                subject_id: id,
                siblings: siblings.clone(),
                season_override,
                audio,
            });
        }
    }

    // Providers whose details failed still get asked for streams by their id.
    for (p, id) in &st.fallback {
        if !id.is_empty()
            && !targets
                .iter()
                .any(|t| t.provider == *p && t.subject_id == *id)
        {
            targets.push(Target {
                provider: *p,
                subject_id: id.clone(),
                siblings: Vec::new(),
                season_override: None,
                audio: None,
            });
        }
    }

    let futs = targets.iter().enumerate().map(|(ti, t)| {
        let svc = ctx.svc.clone();
        let p = t.provider;
        let id = t.subject_id.clone();
        let (season, episode) = (t.season_override.unwrap_or(st.season), st.episode);
        async move {
            let limit = Duration::from_secs(if p == ProviderKind::Addons { 60 } else { 45 });
            let mut r =
                tokio::time::timeout(limit, provider_streams(&svc, p, &id, season, episode)).await;
            // One more try after a network hiccup.
            if matches!(r, Ok(Err(ProviderError::Network(_)))) {
                tokio::time::sleep(Duration::from_millis(800)).await;
                r = tokio::time::timeout(limit, provider_streams(&svc, p, &id, season, episode))
                    .await;
            }
            (ti, p, r)
        }
    });
    let outs = futures::future::join_all(futs).await;

    let mut tagged: Vec<(Release, usize)> = Vec::new();
    let mut errs: Vec<String> = Vec::new();
    let mut summary: Vec<(ProviderKind, Summary)> = Vec::new();
    for t in &targets {
        if !summary.iter().any(|(p, _)| *p == t.provider) {
            summary.push((t.provider, Summary::default()));
        }
    }
    for (ti, p, r) in outs {
        match r {
            Ok(Ok(v)) => {
                eprintln!("[streams] {} -> {} links", p.label(), v.len());
                if let Some(sm) = summary_mut(&mut summary, p) {
                    sm.found += v.len();
                    if v.is_empty() && sm.why.is_none() {
                        sm.why = Some("no streams".to_string());
                    }
                }
                if v.is_empty() {
                    errs.push(format!("{}: no streams", p.label()));
                }
                tagged.extend(v.into_iter().map(|x| (x, ti)));
            }
            Ok(Err(ProviderError::NotFound)) => {
                eprintln!("[streams] {} -> not found", p.label());
                if let Some(sm) = summary_mut(&mut summary, p) {
                    sm.why.get_or_insert_with(|| "no streams found".to_string());
                }
                errs.push(format!("{}: no streams found", p.label()))
            }
            Ok(Err(e)) => {
                eprintln!("[streams] {} failed: {e:?}", p.label());
                let msg = e.user_message(p);
                if let Some(sm) = summary_mut(&mut summary, p) {
                    sm.why.get_or_insert_with(|| msg.clone());
                }
                errs.push(msg)
            }
            Err(_) => {
                eprintln!("[streams] {} timed out", p.label());
                if let Some(sm) = summary_mut(&mut summary, p) {
                    sm.why.get_or_insert_with(|| "timed out".to_string());
                }
                errs.push(format!("{} timed out.", p.label()))
            }
        }
    }
    let found_before_check = tagged.len();
    // Hide dead links (404, error pages, ...). 4KHDHub pages are resolved and
    // checked later, when the quality is tapped.
    {
        use futures::StreamExt;
        let mut jobs: Vec<ProbeJob> = Vec::new();
        for (i, (rel, _)) in tagged.iter().enumerate() {
            if rel.provider == ProviderKind::FourKHdHub {
                continue;
            }
            for (mi, m) in rel.mirrors.iter().enumerate().take(2) {
                jobs.push((i, mi, m.resolver_url.clone(), m.headers.clone()));
            }
        }
        let checked: Vec<(usize, usize, Result<bool, String>)> = futures::stream::iter(jobs)
            .map(|(i, mi, url, headers)| async move {
                (i, mi, link_status(probe_client(), &url, &headers).await)
            })
            .buffer_unordered(40)
            .collect()
            .await;
        let count_of = |tagged: &[(Release, usize)], p: ProviderKind| {
            tagged.iter().filter(|(r, _)| r.provider == p).count()
        };
        let before: Vec<(ProviderKind, usize)> = summary
            .iter()
            .map(|(p, _)| (*p, count_of(&tagged, *p)))
            .collect();
        let mut dead: std::collections::HashSet<(usize, usize)> = std::collections::HashSet::new();
        for (i, mi, res) in &checked {
            if let Err(reason) = res {
                dead.insert((*i, *mi));
                if let Some(sm) = summary_mut(&mut summary, tagged[*i].0.provider) {
                    sm.hidden_why.get_or_insert_with(|| reason.clone());
                }
            }
        }
        for (i, (rel, _)) in tagged.iter_mut().enumerate() {
            if rel.provider == ProviderKind::FourKHdHub {
                continue;
            }
            let mut mi = 0usize;
            rel.mirrors.retain(|_| {
                let keep = !dead.contains(&(i, mi));
                mi += 1;
                keep
            });
        }
        tagged
            .retain(|(rel, _)| rel.provider == ProviderKind::FourKHdHub || !rel.mirrors.is_empty());
        for (p, was) in before {
            let now = count_of(&tagged, p);
            if let Some(sm) = summary_mut(&mut summary, p) {
                sm.hidden = was.saturating_sub(now);
            }
        }
    }
    let asked: Vec<ProviderKind> = targets.iter().map(|t| t.provider).collect();
    let absent: Vec<&str> = BOT_PROVIDERS
        .iter()
        .filter(|p| !p.is_bdix() && !asked.contains(p))
        .map(|p| p.label())
        .collect();
    let streams_note = sources_note(
        &summary,
        &|p| tagged.iter().filter(|(r, _)| r.provider == p).count(),
        &absent,
    );
    if tagged.is_empty() {
        let mut text = "⚠️ No streams found for this item.".to_string();
        if found_before_check > 0 {
            errs.push(format!(
                "{found_before_check} link(s) found, but none passed the check (HTTP 200 and over 50 MB)"
            ));
        }
        errs.dedup();
        for e in errs.iter().take(8) {
            text.push_str(&format!("\n• {}", esc(&short(e, 140))));
        }
        if !absent.is_empty() {
            text.push_str(&format!("\n• Title not found on: {}", absent.join(", ")));
        }
        show(ctx, chat, target_msg, &text, None).await;
        return;
    }
    // MovieBox does not say which language a source is in: take it from the dub.
    for (rel, ti) in tagged.iter_mut() {
        if rel.provider == ProviderKind::MovieBox && language_of(rel).is_none() {
            let audio = targets.get(*ti).and_then(|t| t.audio.clone());
            rel.language = Some(audio.unwrap_or_else(|| "Original audio".to_string()));
        }
    }
    tagged.sort_by(|a, b| {
        b.0.resolution_u64()
            .cmp(&a.0.resolution_u64())
            .then_with(|| b.0.size_bytes.cmp(&a.0.size_bytes))
    });
    let (releases, rel_target): (Vec<Release>, Vec<usize>) = tagged.into_iter().unzip();
    st_set(ctx, chat, |s| {
        s.targets = targets;
        s.releases = releases;
        s.rel_target = rel_target;
        s.streams_note = streams_note;
        s.dead.clear();
    });
    show_qualities(ctx, chat, target_msg).await;
}

async fn show_qualities(ctx: &Ctx, chat: i64, msg: Option<i64>) {
    let st = st_get(ctx, chat);
    let mut rows: Vec<Vec<Value>> = Vec::new();
    let live: Vec<(usize, &Release)> = st
        .releases
        .iter()
        .enumerate()
        .filter(|(i, _)| !st.dead.contains(i))
        .collect();
    for (n, (i, r)) in live.iter().take(MAX_QUALITY_BUTTONS).enumerate() {
        rows.push(vec![cb(
            &short(&format!("{}. {}", n + 1, rel_short(r)), 60),
            format!("r:{i}"),
        )]);
    }
    // Telegram cannot scroll text on buttons, so the full details of every
    // option are listed in the message (long lines wrap onto more lines).
    let mut text = format!(
        "<b>{}</b>{}\nChoose quality / source ({} available):\n",
        esc(&st.title),
        esc(&episode_tag(&st)),
        live.len()
    );
    let mut shown = 0usize;
    let limit = 3500usize.saturating_sub(st.streams_note.len());
    for (n, (_, r)) in live.iter().take(MAX_QUALITY_BUTTONS).enumerate() {
        let line = format!("\n{}. {}", n + 1, esc(&rel_full(r)));
        if text.len() + line.len() > limit {
            break;
        }
        text.push_str(&line);
        shown += 1;
    }
    if shown < live.len().min(MAX_QUALITY_BUTTONS) {
        text.push_str("\n\n… more options are in the buttons below.");
    }
    if !st.streams_note.is_empty() {
        text.push_str(&format!("\n\n{}", esc(&st.streams_note)));
    }
    show(ctx, chat, msg, &text, Some(kb(rows))).await;
}

// ------------------------------------------------------------ Link checking

/// (release index, mirror index, url, headers)
type ProbeJob = (usize, usize, String, Vec<(String, String)>);

fn probe_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(6))
            .user_agent("Mozilla/5.0 (Linux; Android 13) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Mobile Safari/537.36")
            .build()
            .unwrap_or_default()
    })
}

/// HLS stream (playlist), which has no file size and cannot be saved as one file.
fn is_hls(url: &str) -> bool {
    url.split(['?', '#'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
        .ends_with(".m3u8")
}

fn host_of(url: &str) -> &str {
    url.split("://")
        .nth(1)
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or("")
}

/// A link is only shown when it passes both tests.
const MIN_LINK_BYTES: u64 = 50 * 1024 * 1024;

/// First bytes of a playlist-like answer (no Range header).
async fn fetch_head(
    http: &reqwest::Client,
    url: &str,
    headers: &[(String, String)],
) -> Option<String> {
    let mut req = http.get(url);
    for (k, v) in headers {
        req = req.header(k.as_str(), v.as_str());
    }
    let mut resp = tokio::time::timeout(Duration::from_secs(10), req.send())
        .await
        .ok()?
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let chunk = tokio::time::timeout(Duration::from_secs(8), resp.chunk())
        .await
        .ok()?
        .ok()??;
    Some(String::from_utf8_lossy(&chunk).to_string())
}

fn is_playlist_text(text: &str) -> bool {
    text.trim_start_matches('\u{feff}')
        .trim_start()
        .starts_with("#EXTM3U")
}

/// Two tests before a link is sent to the user:
/// 1. the link answers HTTP 200 (206 when the host honours our Range request)
/// 2. the file is bigger than 50 MB
///
/// HLS streams (playlists of small pieces) have no single file size, so they
/// pass when the playlist is valid. Ok(true) means HLS, Ok(false) a plain file.
/// Err(reason) means the link is hidden.
async fn link_status(
    http: &reqwest::Client,
    url: &str,
    headers: &[(String, String)],
) -> Result<bool, String> {
    let mut req = http.get(url).header("Range", "bytes=0-1");
    for (k, v) in headers {
        req = req.header(k.as_str(), v.as_str());
    }
    let resp = match tokio::time::timeout(Duration::from_secs(10), req.send()).await {
        Err(_) => return Err("no answer (timed out)".to_string()),
        Ok(Err(_)) => return Err("cannot connect".to_string()),
        Ok(Ok(r)) => r,
    };

    // Test 1: HTTP 200
    let status = resp.status().as_u16();
    if status != 200 && status != 206 {
        return Err(format!("HTTP {status}"));
    }

    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let total = if status == 206 {
        resp.headers()
            .get("content-range")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.rsplit('/').next())
            .and_then(|v| v.trim().parse::<u64>().ok())
    } else {
        resp.headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
    };

    // HLS: a small answer that is really a playlist.
    let hint = is_hls(url) || ct.contains("mpegurl");
    if hint || total.is_some_and(|t| t < 2 * 1024 * 1024) {
        drop(resp);
        if fetch_head(http, url, headers)
            .await
            .is_some_and(|t| is_playlist_text(&t))
        {
            return Ok(true);
        }
        if hint {
            return Err("the HLS playlist is empty or invalid".to_string());
        }
    }

    // Test 2: size greater than 50 MB
    match total {
        Some(t) if t > MIN_LINK_BYTES => Ok(false),
        Some(t) => Err(format!("file is only {} MB", t / (1024 * 1024))),
        None => Err("file size unknown".to_string()),
    }
}

// ------------------------------------------------------------------ Links

/// (label, url, headers)
type LinkEntry = (String, String, Vec<(String, String)>);

async fn send_links(ctx: &Ctx, chat: i64, k: usize) {
    let st = st_get(ctx, chat);
    let Some(rel) = st.releases.get(k).cloned() else {
        expired(ctx, chat).await;
        return;
    };
    let wait = ctx.tg.send(chat, "⏳ Getting links…", None).await.ok();

    // (label, url, headers)
    let mut links: Vec<LinkEntry> = Vec::new();
    let mut problem: Option<String> = None;

    if rel.provider == ProviderKind::FourKHdHub {
        match ctx.svc.fourk_client.as_ref() {
            Some(c) => {
                let r = tokio::time::timeout(
                    Duration::from_secs(30),
                    c.resolve_release(&rel, ResolutionIntent::Download),
                )
                .await;
                match r {
                    Ok(Ok(src)) => links.push((src.source_label, src.url, src.headers)),
                    Ok(Err(e)) => problem = Some(e.user_message().to_string()),
                    Err(_) => problem = Some("Link resolver timed out.".to_string()),
                }
            }
            None => problem = Some("4KHDHub is unavailable.".to_string()),
        }
    } else {
        for m in rel.mirrors.iter().take(6) {
            links.push((m.label.clone(), m.resolver_url.clone(), m.headers.clone()));
        }
        if links.is_empty() {
            problem = Some("This release has no links.".to_string());
        }
    }

    if links.is_empty() {
        show(
            ctx,
            chat,
            wait,
            &format!(
                "⚠️ {}",
                esc(&problem.unwrap_or_else(|| "No links.".to_string()))
            ),
            None,
        )
        .await;
        return;
    }

    // Drop links that are dead so Play / Download never get them.
    let checks = futures::future::join_all(
        links
            .iter()
            .map(|(_, url, headers)| link_status(probe_client(), url, headers)),
    )
    .await;
    let mut reason: Option<String> = None;
    let mut alive: Vec<(LinkEntry, bool)> = Vec::new();
    for (l, c) in links.into_iter().zip(checks) {
        match c {
            Ok(hls) => alive.push((l, hls)),
            Err(e) => {
                if reason.is_none() {
                    reason = Some(format!("{e} on {}", host_of(&l.1)));
                }
            }
        }
    }
    let links = alive;
    if links.is_empty() {
        st_set(ctx, chat, |s| {
            if !s.dead.contains(&k) {
                s.dead.push(k);
            }
        });
        let rows = vec![vec![cb("⬅ Other qualities", "q".to_string())]];
        let why = reason.unwrap_or_else(|| "no working link".to_string());
        show(
            ctx,
            chat,
            wait,
            &format!(
                "⚠️ This link is not working ({}), so I removed it from the list. Please pick another quality.",
                esc(&why)
            ),
            Some(kb(rows)),
        )
        .await;
        return;
    }

    let title = st.title.clone();
    let tgt = st
        .rel_target
        .get(k)
        .and_then(|ti| st.targets.get(*ti))
        .cloned();
    let mut text = format!(
        "<b>{}</b>{}\n{}\n",
        esc(&title),
        esc(&episode_tag(&st)),
        esc(&rel_full(&rel))
    );
    let base = ctx.base.lock().ok().and_then(|g| g.clone());
    let Some(base) = base else {
        show(
            ctx,
            chat,
            wait,
            "⏳ The player is still starting up. Please tap the quality again in a minute.",
            None,
        )
        .await;
        return;
    };
    let mut items: Vec<StreamItem> = Vec::new();
    for (i, ((label, url, headers), hls)) in links.iter().enumerate() {
        let name = if label.trim().is_empty() {
            format!("Link {}", i + 1)
        } else {
            short(label, 30)
        };
        text.push_str(&format!("\n• {}", esc(&name)));
        items.push(StreamItem {
            name,
            url: url.clone(),
            headers: headers.clone(),
            hls: *hls,
        });
    }
    text.push('\n');

    // Subtitles (MovieBox)
    if rel.provider == ProviderKind::MovieBox {
        if let (Some(rid), Some(t)) = (rel.resource_id.as_ref(), tgt.as_ref()) {
            let caps = tokio::time::timeout(
                Duration::from_secs(15),
                ctx.svc.get_ext_captions(
                    &t.subject_id,
                    rid,
                    &t.siblings,
                    t.season_override.unwrap_or(st.season),
                    st.episode,
                ),
            )
            .await;
            if let Ok(Ok(list)) = caps {
                if !list.is_empty() {
                    text.push_str("\n<b>Subtitles</b>\n");
                    for s in list.iter().take(12) {
                        text.push_str(&format!(
                            "• <a href=\"{}\">{}</a>\n",
                            esc(&s.url).replace('"', "%22"),
                            esc(&short(&s.name, 40))
                        ));
                    }
                }
            }
        }
    }

    let sid = random_id();
    let page = format!("{}/p/{}", base.trim_end_matches('/'), sid);
    let base_text = short_html(&text);
    let locked_text = format!(
        "{base_text}\n\n🔒 <b>Watch {} to unlock Play and Download.</b>",
        ads_word(ADS_REQUIRED)
    );
    insert_stream(
        &ctx.streams,
        &sid,
        Stream {
            items,
            title: format!("{}{}", title, episode_tag(&st)),
            file_base: format!(
                "{}{} {}",
                title,
                episode_tag(&st),
                rel.quality.clone().unwrap_or_default()
            ),
            chat,
            msg: 0,
            text: base_text,
            page: page.clone(),
            unlocked_at: None,
        },
    );

    if let Some(w) = wait {
        let _ = ctx
            .tg
            .call("deleteMessage", &json!({"chat_id": chat, "message_id": w}))
            .await;
    }
    let back = vec![cb("⬅ Other qualities", "q".to_string())];
    let with_webapp = kb(vec![
        vec![
            json!({"text": format!("🔓 Watch {} to unlock", ads_word(ADS_REQUIRED)), "web_app": {"url": page.clone()}}),
        ],
        back.clone(),
    ]);
    match ctx.tg.send(chat, &locked_text, Some(with_webapp)).await {
        Ok(mid) => set_stream_msg(&ctx.streams, &sid, mid),
        Err(_) => {
            // Mini-App buttons only work in private chats; fall back to a link button.
            let with_url = kb(vec![
                vec![
                    json!({"text": format!("🔓 Watch {} to unlock", ads_word(ADS_REQUIRED)), "url": page.clone()}),
                ],
                back,
            ]);
            if let Ok(mid) = ctx.tg.send(chat, &locked_text, Some(with_url)).await {
                set_stream_msg(&ctx.streams, &sid, mid);
            }
        }
    }
}

/// Telegram messages are limited to 4096 chars.
fn short_html(s: &str) -> String {
    if s.chars().count() <= 4000 {
        s.to_string()
    } else {
        let t: String = s.chars().take(3990).collect();
        // avoid cutting inside an HTML tag/entity
        let cut = t.rfind('\n').unwrap_or(t.len());
        t[..cut].to_string()
    }
}

// --------------------------------------------------------------- Dispatch

fn authorised(ctx: &Ctx, chat: i64) -> bool {
    ctx.allowed.is_empty() || ctx.allowed.contains(&chat)
}

async fn handle_message(ctx: Ctx, msg: Value) {
    let Some(chat) = msg.pointer("/chat/id").and_then(Value::as_i64) else {
        return;
    };
    let text = msg
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if text.is_empty() {
        return;
    }
    if text.starts_with("/id") {
        let _ = ctx
            .tg
            .send(chat, &format!("Chat id: <code>{chat}</code>"), None)
            .await;
        return;
    }
    if !authorised(&ctx, chat) {
        let _ = ctx
            .tg
            .send(chat, "⛔ You are not allowed to use this bot.", None)
            .await;
        return;
    }
    if text.starts_with('/') {
        let (cmd, rest) = match text.split_once(char::is_whitespace) {
            Some((c, r)) => (c.to_string(), r.trim().to_string()),
            None => (text.clone(), String::new()),
        };
        let cmd = cmd.split('@').next().unwrap_or("").to_lowercase();
        match cmd.as_str() {
            "/start" | "/help" => {
                let _ = ctx.tg.send(chat, HELP, None).await;
            }
            "/search" | "/s" => {
                if rest.is_empty() {
                    let _ = ctx
                        .tg
                        .send(chat, "Usage: /search &lt;movie or series name&gt;", None)
                        .await;
                } else {
                    do_search(&ctx, chat, &rest).await;
                }
            }
            _ => {
                let _ = ctx.tg.send(chat, HELP, None).await;
            }
        }
        return;
    }
    do_search(&ctx, chat, &text).await;
}

async fn handle_callback(ctx: Ctx, cq: Value) {
    let id = cq
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    ctx.tg.answer(&id, "").await;
    let Some(chat) = cq.pointer("/message/chat/id").and_then(Value::as_i64) else {
        return;
    };
    if !authorised(&ctx, chat) {
        return;
    }
    let msg_id = cq.pointer("/message/message_id").and_then(Value::as_i64);
    let data = cq
        .get("data")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let parts: Vec<&str> = data.split(':').collect();
    let num = |i: usize| parts.get(i).and_then(|s| s.parse::<usize>().ok());

    match parts.first().copied().unwrap_or("") {
        "g" => {
            if let Some(page) = num(1) {
                show_results(&ctx, chat, msg_id, page).await;
            }
        }
        "d" => {
            if let Some(gi) = num(1) {
                open_group(&ctx, chat, gi).await;
            }
        }
        "ss" => show_seasons(&ctx, chat, msg_id).await,
        "s" => {
            if let (Some(season), Some(page)) = (num(1), num(2)) {
                show_episodes(&ctx, chat, msg_id, season, page).await;
            }
        }
        "e" => {
            if let (Some(season), Some(ep)) = (num(1), num(2)) {
                st_set(&ctx, chat, |s| {
                    s.season = season;
                    s.episode = ep;
                    s.targets.clear();
                    s.releases.clear();
                    s.rel_target.clear();
                    s.dead.clear();
                });
                show_audio(&ctx, chat, msg_id).await;
            }
        }
        "al" => fetch_all(&ctx, chat, msg_id).await,
        "q" => show_qualities(&ctx, chat, None).await,
        "r" => {
            if let Some(k) = num(1) {
                send_links(&ctx, chat, k).await;
            }
        }
        _ => {}
    }
}

async fn handle_update(ctx: Ctx, upd: Value) {
    if let Some(m) = upd.get("message").cloned() {
        handle_message(ctx, m).await;
    } else if let Some(c) = upd.get("callback_query").cloned() {
        handle_callback(ctx, c).await;
    }
}

// ----------------------------------------------- Built-in player/stream server

#[derive(Clone)]
struct StreamItem {
    name: String,
    url: String,
    headers: Vec<(String, String)>,
    /// HLS stream (playlist of small pieces)
    hls: bool,
}

#[derive(Clone)]
struct Stream {
    items: Vec<StreamItem>,
    title: String,
    /// base name of the downloaded file (title + episode + quality)
    file_base: String,
    chat: i64,
    msg: i64,
    /// message text without the lock/unlock status line
    text: String,
    /// https://host/p/ID
    page: String,
    unlocked_at: Option<Instant>,
}

/// After watching the ads, playback stays unlocked for 6 hours.
const UNLOCK_SECS: u64 = 6 * 3600;

impl Stream {
    fn is_unlocked(&self) -> bool {
        self.unlocked_at
            .map(|t| t.elapsed() < Duration::from_secs(UNLOCK_SECS))
            .unwrap_or(false)
    }
}

type StreamMap = Arc<Mutex<HashMap<String, Stream>>>;

fn random_id() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    // RandomState is seeded from the OS RNG on every platform.
    let mut out = String::new();
    for i in 0u8..2 {
        let mut h = RandomState::new().build_hasher();
        h.write_u8(i);
        h.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        );
        out.push_str(&format!("{:016x}", h.finish()));
    }
    out
}

fn insert_stream(map: &StreamMap, id: &str, st: Stream) {
    if let Ok(mut g) = map.lock() {
        if g.len() > 5000 {
            g.clear();
        }
        g.insert(id.to_string(), st);
    }
}

fn set_stream_msg(map: &StreamMap, id: &str, msg: i64) {
    if let Ok(mut g) = map.lock() {
        if let Some(st) = g.get_mut(id) {
            st.msg = msg;
        }
    }
}

/// https://host/p/ID  ->  https://host/d/ID?n=N  (file download link)
fn dl_url(page: &str, n: usize) -> String {
    match page.rfind("/p/") {
        Some(i) => format!("{}/d/{}?n={}", &page[..i], &page[i + 3..], n),
        None => format!("{page}?n={n}"),
    }
}

/// File extension for a download: from the link, else from the content type.
fn file_ext(url: &str, ctype: &str) -> &'static str {
    let path = url.split(['?', '#']).next().unwrap_or("");
    let last = path.rsplit('/').next().unwrap_or("");
    let ext = last
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    for known in ["mkv", "mp4", "avi", "webm", "mov", "m4v", "ts"] {
        if ext == known {
            return known;
        }
    }
    let ct = ctype.to_ascii_lowercase();
    if ct.contains("matroska") {
        "mkv"
    } else if ct.contains("webm") {
        "webm"
    } else {
        "mp4"
    }
}

/// Safe ASCII file name for the Content-Disposition header.
fn download_name(base: &str, ext: &str) -> String {
    let mut name = String::new();
    for c in base.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.' | '(' | ')') {
            name.push(c);
        } else {
            name.push('_');
        }
    }
    let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
    let name = name.trim_matches(|c| c == '_' || c == ' ' || c == '.');
    let name = if name.is_empty() { "video" } else { name };
    format!("{name}.{ext}")
}

/// Play buttons that appear after the ads were watched.
/// Play / Download buttons. `copy` adds "Copy link" (needs a recent Telegram app).
fn play_keyboard(st: &Stream, webapp: bool, copy: bool) -> Value {
    let mut rows: Vec<Vec<Value>> = Vec::new();
    for (i, it) in st.items.iter().enumerate() {
        let page = format!("{}?n={}", st.page, i);
        let label = format!("▶️ Play: {}", short(&it.name, 30));
        if webapp {
            rows.push(vec![
                json!({"text": label, "web_app": {"url": page.clone()}}),
            ]);
        } else {
            rows.push(vec![json!({"text": label, "url": page.clone()})]);
        }
        rows.push(vec![
            json!({"text": "📱 MX Player", "url": format!("{page}&go=mx")}),
            json!({"text": "🎞 VLC", "url": format!("{page}&go=vlc")}),
        ]);
        let dl = dl_url(&st.page, i);
        let mut dl_row = vec![json!({"text": "⬇️ Download", "url": dl.clone()})];
        if copy {
            // Tapping copies the direct download link, for a download manager app.
            dl_row.push(json!({"text": "📋 Copy link", "copy_text": {"text": dl}}));
        }
        rows.push(dl_row);
    }
    rows.push(vec![cb("⬅ Other qualities", "q".to_string())]);
    kb(rows)
}

/// Shown when a link is not known any more (the bot restarted or 6 hours passed).
const LINK_GONE_TEXT: &[u8] = b"This link is no longer active: the bot was restarted or the 6 hours ended. Go back to the bot and tap the quality again.";

/// How often the countdown in the message is refreshed (Telegram limits how
/// fast a bot may edit a message, so it ticks every few seconds).
const COUNTDOWN_TICK_SECS: u64 = 5;

/// 7325 -> "02:02:05" (hours:minutes:seconds)
fn fmt_hms(secs: u64) -> String {
    format!(
        "{:02}:{:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

fn unlocked_text(text: &str, secs_left: u64) -> String {
    format!(
        "{text}\n\n✅ <b>Unlocked.</b> Tap Play or Download below.\n📋 Copy link = paste it into a download manager app (ADM, 1DM, IDM).\n⏱ Time left: <b>{}</b> (hr:min:sec)",
        fmt_hms(secs_left)
    )
}

/// After unlocking: count down in the message (hr:min:sec), and when the time
/// is over remove the Play buttons and the stream.
#[allow(clippy::too_many_arguments)]
async fn countdown_task(
    tg: Tg,
    streams: StreamMap,
    id: String,
    chat: i64,
    msg: i64,
    text: String,
    keyboard: Value,
    deadline: Instant,
) {
    let mut editing = true;
    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let left = deadline - now;
        if !editing {
            // Message was deleted: just wait for the end.
            tokio::time::sleep(left).await;
            break;
        }
        tokio::time::sleep(Duration::from_secs(COUNTDOWN_TICK_SECS).min(left)).await;
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let secs_left = (deadline - now).as_secs();
        let t = unlocked_text(&text, secs_left);
        if let Err(e) = tg.edit(chat, msg, &t, Some(keyboard.clone())).await {
            let e = e.to_ascii_lowercase();
            if e.contains("not found")
                || e.contains("message_id_invalid")
                || e.contains("can't be edited")
            {
                editing = false;
            } else if e.contains("too many requests") || e.contains("retry after") {
                tokio::time::sleep(Duration::from_secs(15)).await;
            }
        }
    }
    if let Ok(mut g) = streams.lock() {
        g.remove(&id);
    }
    let t = format!(
        "{text}\n\n⌛ <b>Expired.</b> Tap a quality again and watch {} to play.",
        ads_word(ADS_REQUIRED)
    );
    let _ = tg
        .edit(
            chat,
            msg,
            &t,
            Some(kb(vec![vec![cb("⬅ Other qualities", "q".to_string())]])),
        )
        .await;
}

const PLAYER_HTML: &str = r##"<!doctype html><html><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1,viewport-fit=cover">
<title>__TITLE__</title>
<script src="https://telegram.org/js/telegram-web-app.js"></script>
__HLS_SCRIPT__
<style>
html,body{margin:0;background:#0f0f14;color:#eee;font-family:system-ui,sans-serif}
h1{font-size:16px;margin:12px;font-weight:600}
#wrap{position:relative;background:#000}
video{display:block;width:100%;max-height:70vh;background:#000}
#fs{position:absolute;top:8px;right:8px;z-index:5;border:0;border-radius:8px;background:rgba(0,0,0,.55);color:#fff;font-size:18px;padding:6px 10px}
.row{display:flex;gap:8px;margin:12px}
a.b{flex:1;text-align:center;padding:12px;border-radius:10px;background:#2b6cff;color:#fff;text-decoration:none;font-weight:600}
a.v{background:#e8730c}
a.d{background:#198754}
p{margin:12px;font-size:13px;color:#aaa}
body.fs h1,body.fs .row,body.fs p{display:none}
body.fs #wrap{position:fixed;top:0;left:0;right:0;bottom:0;z-index:99;width:100vw;height:100vh}
body.fs video{width:100vw;height:100vh;max-height:none;object-fit:contain}
</style></head><body>
<h1>__TITLE__</h1>
<div id="wrap">
<video id="v" controls controlslist="nofullscreen" autoplay playsinline webkit-playsinline></video>
<button id="fs" type="button">&#x26F6;</button>
</div>
<div class="row"><a class="b" id="mx" href="#">MX Player</a><a class="b v" id="vlc" href="#">VLC</a><a class="b d" id="dl" href="#">Download</a></div>
<p id="msg">If the video does not start here (some formats are not supported by the in-app player), use MX Player or VLC.</p>
<script>
var TITLE=__TITLE_JS__, GO="__GO__";
var tg=window.Telegram&&window.Telegram.WebApp;
if(tg){try{tg.ready();tg.expand();}catch(e){}}
var inTg=!!(tg&&tg.initData);
var abs=location.origin+"/s/__ID__?n=__N__";
function intentFor(pkg){return "intent://"+abs.replace(/^https?:\/\//,"")+"#Intent;scheme=https;package="+pkg+";type=video/*;S.title="+encodeURIComponent(TITLE)+";end";}
var PK={mx:"com.mxtech.videoplayer.ad",vlc:"org.videolan.vlc"};
function launch(k){
  if(inTg&&tg.openLink){tg.openLink(location.origin+"/p/__ID__?n=__N__&go="+k);return;}
  location.href=intentFor(PK[k]);
}
document.getElementById("mx").onclick=function(e){e.preventDefault();launch("mx");};
document.getElementById("vlc").onclick=function(e){e.preventDefault();launch("vlc");};
document.getElementById("dl").onclick=function(e){e.preventDefault();var u=location.origin+"/d/__ID__?n=__N__";if(inTg&&tg.openLink){tg.openLink(u);}else{location.href=u;}};
var SRC="/s/__ID__?n=__N__",HLS=__HLS__,vid=document.getElementById("v");
if(HLS&&window.Hls&&Hls.isSupported()){var hl=new Hls();hl.loadSource(SRC);hl.attachMedia(vid);hl.on(Hls.Events.ERROR,function(e,d){if(d&&d.fatal){document.getElementById("msg").textContent="This stream cannot play in-app. Tap MX Player or VLC above.";}});}
else{vid.src=SRC;}
document.getElementById("v").addEventListener("error",function(){document.getElementById("msg").textContent="This video format cannot play in-app. Tap MX Player or VLC above.";});
/* Fullscreen: Telegram's webview blocks the native button, so use our own. */
var wrap=document.getElementById("wrap");
function fsOn(){document.body.classList.add("fs");try{if(tg&&tg.requestFullscreen)tg.requestFullscreen();}catch(e){}}
function fsOff(){document.body.classList.remove("fs");try{if(tg&&tg.exitFullscreen)tg.exitFullscreen();}catch(e){}try{if(document.fullscreenElement&&document.exitFullscreen)document.exitFullscreen();}catch(e){}}
document.getElementById("fs").onclick=function(){
  if(document.body.classList.contains("fs")){fsOff();return;}
  if(inTg){fsOn();return;}
  var r=wrap.requestFullscreen||wrap.webkitRequestFullscreen;
  if(r){try{var pr=r.call(wrap);if(pr&&pr.catch){pr.catch(function(){fsOn();});}}catch(e){fsOn();}}else{fsOn();}
};
document.addEventListener("fullscreenchange",function(){if(!document.fullscreenElement){document.body.classList.remove("fs");}});
if(GO&&PK[GO]&&!inTg){document.getElementById("v").pause();setTimeout(function(){location.href=intentFor(PK[GO]);},300);}
</script></body></html>"##;

const GATE_HTML: &str = r##"<!doctype html><html><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Unlock</title>
<script src="https://telegram.org/js/telegram-web-app.js"></script>
<script src='//libtl.com/sdk.js' data-zone='12229158' data-sdk='show_12229158'></script>
<style>
body{margin:0;background:#0f0f14;color:#eee;font-family:system-ui,sans-serif}
h1{font-size:16px;margin:14px;font-weight:600}
.card{margin:14px;padding:18px;border-radius:14px;background:#1b1b26;text-align:center}
h2{margin:0 0 8px;font-size:18px}
p{font-size:14px;color:#bbb}
#prog{font-size:16px;margin:12px 0;font-weight:600}
button{width:100%;padding:14px;border:0;border-radius:12px;background:#2b6cff;color:#fff;font-size:16px;font-weight:700}
button:disabled{opacity:.6}
#msg{min-height:20px;color:#ffb74d}
</style></head><body>
<h1>__TITLE__</h1>
<div class="card">
<h2>&#x1F512; Unlock playback</h2>
<p>Watch __ADS_TEXT__ to unlock the video and the download. It stays unlocked for 6 hours.</p>
<div id="prog">Ads watched: 0 / __ADS_N__</div>
<button id="btn" type="button">&#x25B6; Watch ad</button>
<p id="msg"></p>
</div>
<script>
var tg=window.Telegram&&window.Telegram.WebApp;
if(tg){try{tg.ready();tg.expand();}catch(e){}}
var done=0,busy=false,N=__ADS_N__;
var btn=document.getElementById("btn"),prog=document.getElementById("prog"),msg=document.getElementById("msg");
function adFn(){return (typeof show_12229158==="function")?show_12229158:null;}
function unlock(){
  msg.textContent="Unlocking...";
  fetch("/unlock/__ID__",{method:"POST"}).then(function(r){
    if(r.ok){location.reload();}else{msg.textContent="Could not unlock. Close this window and try again.";}
  }).catch(function(){msg.textContent="Network error. Please reopen and try again.";});
}
btn.onclick=function(){
  if(busy)return;
  var f=adFn();
  if(!f){msg.textContent="The ad could not load. Check your connection, disable any ad-blocker and try again.";return;}
  busy=true;btn.disabled=true;msg.textContent="Loading ad...";
  f().then(function(){
    done++;prog.textContent="Ads watched: "+done+" / "+N;busy=false;btn.disabled=false;msg.textContent="";
    if(done>=N){btn.style.display="none";unlock();}
    else{btn.textContent="\u25B6 Watch ad "+(done+1)+" of "+N;}
  }).catch(function(){busy=false;btn.disabled=false;msg.textContent="The ad was not completed. Please watch it fully.";});
};
</script></body></html>"##;

fn js_str(s: &str) -> String {
    serde_json::to_string(s)
        .unwrap_or_else(|_| "\"\"".to_string())
        .replace("</", "<\\/")
}

fn player_page(id: &str, title: &str, go: &str, n: usize, hls: bool) -> String {
    let go = if go == "mx" || go == "vlc" { go } else { "" };
    PLAYER_HTML
        .replace("__TITLE_JS__", &js_str(title))
        .replace("__TITLE__", &esc(title))
        .replace("__ID__", id)
        .replace("__N__", &n.to_string())
        .replace("__GO__", go)
        .replace(
            "__HLS_SCRIPT__",
            if hls {
                r#"<script src="https://cdn.jsdelivr.net/npm/hls.js@1.5.17/dist/hls.min.js"></script>"#
            } else {
                ""
            },
        )
        .replace("__HLS__", if hls { "true" } else { "false" })
}

fn gate_page(id: &str, title: &str) -> String {
    GATE_HTML
        .replace("__TITLE__", &esc(title))
        .replace("__ID__", id)
        .replace("__ADS_TEXT__", &ads_word(ADS_REQUIRED))
        .replace("__ADS_N__", &ADS_REQUIRED.to_string())
}

async fn http_simple(
    sock: &mut tokio::net::TcpStream,
    code: u16,
    reason: &str,
    ctype: &str,
    body: &[u8],
) {
    use tokio::io::AsyncWriteExt;
    let head = format!(
        "HTTP/1.1 {code} {reason}\r\ncontent-type: {ctype}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    let _ = sock.write_all(head.as_bytes()).await;
    let _ = sock.write_all(body).await;
    let _ = sock.flush().await;
}

async fn serve_conn(
    mut sock: tokio::net::TcpStream,
    streams: StreamMap,
    http: reqwest::Client,
    tg: Tg,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 2048];
    loop {
        match sock.read(&mut tmp).await {
            Ok(0) => return,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
                if buf.len() > 16384 {
                    return;
                }
            }
            Err(_) => return,
        }
    }
    let head = String::from_utf8_lossy(&buf).to_string();
    let mut lines = head.split("\r\n");
    let first = lines.next().unwrap_or("").to_string();
    let mut it = first.split_whitespace();
    let method = it.next().unwrap_or("").to_string();
    let target = it.next().unwrap_or("/").to_string();
    let mut range: Option<String> = None;
    for l in lines {
        if let Some((n, v)) = l.split_once(':') {
            if n.trim().eq_ignore_ascii_case("range") {
                range = Some(v.trim().to_string());
            }
        }
    }
    let (path, query) = target.split_once('?').unwrap_or((target.as_str(), ""));
    // For uptime monitors (UptimeRobot, Render health check).
    if path == "/health" || path == "/" {
        http_simple(&mut sock, 200, "OK", "text/plain", b"ok").await;
        return;
    }
    let get = |id: &str| -> Option<Stream> { streams.lock().ok().and_then(|g| g.get(id).cloned()) };

    let n: usize = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("n="))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    // Called by the gate page after the required ads were watched.
    if method == "POST" {
        if let Some(id) = path.strip_prefix("/unlock/") {
            let snapshot: Option<(Stream, bool)> = match streams.lock() {
                Ok(mut g) => match g.get_mut(id) {
                    Some(st) => {
                        let newly = !st.is_unlocked();
                        if newly {
                            st.unlocked_at = Some(Instant::now());
                        }
                        Some((st.clone(), newly))
                    }
                    None => None,
                },
                Err(_) => None,
            };
            let Some((st, newly)) = snapshot else {
                http_simple(&mut sock, 404, "Not Found", "text/plain", LINK_GONE_TEXT).await;
                return;
            };
            if newly && st.msg != 0 {
                let deadline =
                    st.unlocked_at.unwrap_or_else(Instant::now) + Duration::from_secs(UNLOCK_SECS);
                let text = unlocked_text(&st.text, UNLOCK_SECS);
                // Best keyboard first; fall back if Telegram refuses a button type.
                let mut keyboard = play_keyboard(&st, true, true);
                for (webapp, copy) in [(false, true), (false, false)] {
                    if tg
                        .edit(st.chat, st.msg, &text, Some(keyboard.clone()))
                        .await
                        .is_ok()
                    {
                        break;
                    }
                    keyboard = play_keyboard(&st, webapp, copy);
                }
                tokio::spawn(countdown_task(
                    tg.clone(),
                    streams.clone(),
                    id.to_string(),
                    st.chat,
                    st.msg,
                    st.text.clone(),
                    keyboard,
                    deadline,
                ));
            }
            http_simple(&mut sock, 200, "OK", "application/json", b"{\"ok\":true}").await;
            return;
        }
    }

    if let Some(id) = path.strip_prefix("/p/") {
        let Some(entry) = get(id) else {
            http_simple(&mut sock, 404, "Not Found", "text/plain", LINK_GONE_TEXT).await;
            return;
        };
        let html = if entry.is_unlocked() {
            let go = query
                .split('&')
                .find_map(|kv| kv.strip_prefix("go="))
                .unwrap_or("");
            let hls = entry.items.get(n).is_some_and(|i| i.hls);
            player_page(id, &entry.title, go, n, hls)
        } else {
            gate_page(id, &entry.title)
        };
        http_simple(
            &mut sock,
            200,
            "OK",
            "text/html; charset=utf-8",
            html.as_bytes(),
        )
        .await;
        return;
    }

    let route = path
        .strip_prefix("/s/")
        .map(|id| (id, 's'))
        .or_else(|| path.strip_prefix("/d/").map(|id| (id, 'd')))
        .or_else(|| path.strip_prefix("/h/").map(|id| (id, 'h')));
    if let Some((id, mode)) = route {
        let download = mode == 'd';
        let Some(entry) = get(id) else {
            http_simple(&mut sock, 404, "Not Found", "text/plain", b"Expired").await;
            return;
        };
        if !entry.is_unlocked() {
            http_simple(
                &mut sock,
                403,
                "Forbidden",
                "text/plain",
                b"Locked: watch the ads first",
            )
            .await;
            return;
        }
        let Some(item) = entry.items.get(n).cloned() else {
            http_simple(&mut sock, 404, "Not Found", "text/plain", b"No such source").await;
            return;
        };
        // Whole HLS stream as one file: join the pieces.
        if download && item.hls {
            hls_download(&mut sock, &http, &item, &entry.file_base).await;
            return;
        }
        // /h/ = a playlist or piece of an HLS stream (address in "u=")
        let target_url = if mode == 'h' {
            let decoded = query
                .split('&')
                .find_map(|kv| kv.strip_prefix("u="))
                .and_then(|v| {
                    use base64::Engine;
                    base64::engine::general_purpose::URL_SAFE_NO_PAD
                        .decode(v.as_bytes())
                        .ok()
                })
                .and_then(|b| String::from_utf8(b).ok());
            let Some(u) = decoded else {
                http_simple(&mut sock, 400, "Bad Request", "text/plain", b"bad address").await;
                return;
            };
            u
        } else {
            item.url.clone()
        };
        let mut req = http.get(&target_url);
        for (hn, hv) in &item.headers {
            req = req.header(hn.as_str(), hv.as_str());
        }
        if let Some(r) = &range {
            req = req.header("Range", r.as_str());
        }
        let mut resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[proxy] upstream error for {}: {e}", host_of(&item.url));
                http_simple(
                    &mut sock,
                    502,
                    "Bad Gateway",
                    "text/plain",
                    b"upstream error",
                )
                .await;
                return;
            }
        };
        let status = resp.status();
        let up_ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        // A playlist: point every piece to this server so players can load it.
        if status.is_success()
            && mode != 'd'
            && ((mode == 's' && item.hls) || is_hls(&target_url) || up_ct.contains("mpegurl"))
        {
            let base_url = resp.url().clone();
            let body = resp.text().await.unwrap_or_default();
            if is_playlist_text(&body) {
                let out_body = rewrite_playlist(&body, &base_url, id, n);
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/vnd.apple.mpegurl\r\ncontent-length: {}\r\naccess-control-allow-origin: *\r\nconnection: close\r\n\r\n",
                    out_body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                if method != "HEAD" {
                    let _ = sock.write_all(out_body.as_bytes()).await;
                }
                let _ = sock.flush().await;
                return;
            }
            http_simple(&mut sock, 502, "Bad Gateway", "text/plain", b"bad playlist").await;
            return;
        }
        if status.as_u16() >= 400 {
            eprintln!(
                "[proxy] upstream HTTP {} for {}",
                status.as_u16(),
                host_of(&target_url)
            );
        }
        let mut out = format!(
            "HTTP/1.1 {} {}\r\n",
            status.as_u16(),
            status.canonical_reason().unwrap_or("OK")
        );
        let mut has_ct = false;
        let mut ctype = String::new();
        for name in [
            "content-type",
            "content-length",
            "content-range",
            "accept-ranges",
            "last-modified",
            "etag",
        ] {
            if let Some(v) = resp.headers().get(name).and_then(|v| v.to_str().ok()) {
                if name == "content-type" {
                    has_ct = true;
                    ctype = v.to_string();
                }
                out.push_str(&format!("{name}: {v}\r\n"));
            }
        }
        if !has_ct {
            out.push_str("content-type: video/mp4\r\n");
        }
        if download {
            let fname = download_name(&entry.file_base, file_ext(&target_url, &ctype));
            out.push_str(&format!(
                "content-disposition: attachment; filename=\"{fname}\"\r\n"
            ));
        }
        out.push_str("access-control-allow-origin: *\r\nconnection: close\r\n\r\n");
        if sock.write_all(out.as_bytes()).await.is_err() {
            return;
        }
        if method == "HEAD" {
            return;
        }
        while let Ok(Some(chunk)) = resp.chunk().await {
            if sock.write_all(&chunk).await.is_err() {
                return;
            }
        }
        let _ = sock.flush().await;
        return;
    }

    http_simple(
        &mut sock,
        200,
        "OK",
        "text/plain",
        b"MahSurat Stream Bot player server",
    )
    .await;
}

// ------------------------------------------------------------- HLS streams

fn proxy_url(id: &str, n: usize, target: &str) -> String {
    use base64::Engine;
    format!(
        "/h/{id}?n={n}&u={}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(target.as_bytes())
    )
}

/// Rewrite `URI="..."` values inside a playlist tag line.
fn rewrite_uri_attrs(line: &str, f: &dyn Fn(&str) -> String) -> String {
    let mut out = String::new();
    let mut rest = line;
    while let Some(i) = rest.find("URI=\"") {
        let start = i + 5;
        out.push_str(&rest[..start]);
        let after = &rest[start..];
        match after.find('"') {
            Some(end) => {
                out.push_str(&f(&after[..end]));
                rest = &after[end..];
            }
            None => {
                rest = after;
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Every address in the playlist becomes a link to this server (which adds the
/// headers the file host needs).
fn rewrite_playlist(text: &str, base: &url::Url, id: &str, n: usize) -> String {
    let conv = |r: &str| -> String {
        match base.join(r) {
            Ok(u) => proxy_url(id, n, u.as_str()),
            Err(_) => r.to_string(),
        }
    };
    let mut out = String::new();
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() {
            out.push('\n');
        } else if t.starts_with('#') {
            out.push_str(&rewrite_uri_attrs(t, &conv));
            out.push('\n');
        } else {
            out.push_str(&conv(t));
            out.push('\n');
        }
    }
    out
}

async fn get_text(
    http: &reqwest::Client,
    url: &str,
    headers: &[(String, String)],
) -> Option<(String, url::Url)> {
    let mut req = http.get(url);
    for (k, v) in headers {
        req = req.header(k.as_str(), v.as_str());
    }
    let resp = tokio::time::timeout(Duration::from_secs(20), req.send())
        .await
        .ok()?
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let base = resp.url().clone();
    let text = resp.text().await.ok()?;
    is_playlist_text(&text).then_some((text, base))
}

/// Pieces of an HLS media playlist: (optional init piece, pieces, encrypted)
fn parse_media_playlist(text: &str, base: &url::Url) -> (Option<String>, Vec<String>, bool) {
    let mut map: Option<String> = None;
    let mut segs: Vec<String> = Vec::new();
    let mut encrypted = false;
    for line in text.lines() {
        let t = line.trim();
        if let Some(attrs) = t.strip_prefix("#EXT-X-KEY:") {
            if !attrs.contains("METHOD=NONE") {
                encrypted = true;
            }
        } else if let Some(attrs) = t.strip_prefix("#EXT-X-MAP:") {
            if let Some(i) = attrs.find("URI=\"") {
                let rest = &attrs[i + 5..];
                if let Some(end) = rest.find('"') {
                    if let Ok(u) = base.join(&rest[..end]) {
                        map = Some(u.to_string());
                    }
                }
            }
        } else if !t.is_empty() && !t.starts_with('#') {
            if let Ok(u) = base.join(t) {
                segs.push(u.to_string());
            }
        }
    }
    (map, segs, encrypted)
}

/// Download an HLS stream as one file: the pieces are fetched one after the
/// other and sent as a single .ts (or .mp4) download.
async fn hls_download(
    sock: &mut tokio::net::TcpStream,
    http: &reqwest::Client,
    item: &StreamItem,
    file_base: &str,
) {
    use tokio::io::AsyncWriteExt;
    let Some((mut text, mut base)) = get_text(http, &item.url, &item.headers).await else {
        http_simple(sock, 502, "Bad Gateway", "text/plain", b"playlist error").await;
        return;
    };
    // Master playlist: take the best quality.
    if text.contains("#EXT-X-STREAM-INF") {
        let mut best: Option<(u64, String)> = None;
        let mut pending: Option<u64> = None;
        for line in text.lines() {
            let t = line.trim();
            if let Some(attrs) = t.strip_prefix("#EXT-X-STREAM-INF:") {
                pending = attrs
                    .split(',')
                    .find_map(|kv| kv.trim().strip_prefix("BANDWIDTH="))
                    .and_then(|v| v.parse().ok());
            } else if !t.is_empty() && !t.starts_with('#') {
                let bw = pending.take().unwrap_or(0);
                if best.as_ref().is_none_or(|(b, _)| bw > *b) {
                    best = Some((bw, t.to_string()));
                }
            }
        }
        let next = best.and_then(|(_, rel)| base.join(&rel).ok());
        let Some(next) = next else {
            http_simple(sock, 502, "Bad Gateway", "text/plain", b"no variant").await;
            return;
        };
        let Some((t2, b2)) = get_text(http, next.as_str(), &item.headers).await else {
            http_simple(sock, 502, "Bad Gateway", "text/plain", b"playlist error").await;
            return;
        };
        text = t2;
        base = b2;
    }
    let (map, segs, encrypted) = parse_media_playlist(&text, &base);
    if encrypted {
        http_simple(
            sock,
            501,
            "Not Implemented",
            "text/plain",
            b"This stream is encrypted and cannot be downloaded. Use Play instead.",
        )
        .await;
        return;
    }
    if segs.is_empty() {
        http_simple(sock, 502, "Bad Gateway", "text/plain", b"empty playlist").await;
        return;
    }
    let fmp4 = map.is_some()
        || segs[0]
            .split(['?', '#'])
            .next()
            .is_some_and(|p| p.ends_with(".m4s") || p.ends_with(".mp4"));
    let (ext, ctype) = if fmp4 {
        ("mp4", "video/mp4")
    } else {
        ("ts", "video/mp2t")
    };
    let head = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: {ctype}\r\ncontent-disposition: attachment; filename=\"{}\"\r\ntransfer-encoding: chunked\r\naccess-control-allow-origin: *\r\nconnection: close\r\n\r\n",
        download_name(file_base, ext)
    );
    if sock.write_all(head.as_bytes()).await.is_err() {
        return;
    }
    for url in map.iter().chain(segs.iter()) {
        let mut data: Option<Vec<u8>> = None;
        for _ in 0..3 {
            let mut req = http.get(url);
            for (k, v) in &item.headers {
                req = req.header(k.as_str(), v.as_str());
            }
            if let Ok(Ok(r)) = tokio::time::timeout(Duration::from_secs(40), req.send()).await {
                if r.status().is_success() {
                    if let Ok(b) = r.bytes().await {
                        data = Some(b.to_vec());
                        break;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let Some(data) = data else {
            eprintln!("[hls] piece failed on {}", host_of(url));
            return; // cut the connection: the download shows as failed
        };
        let size = format!("{:x}\r\n", data.len());
        if sock.write_all(size.as_bytes()).await.is_err()
            || sock.write_all(&data).await.is_err()
            || sock.write_all(b"\r\n").await.is_err()
        {
            return;
        }
    }
    let _ = sock.write_all(b"0\r\n\r\n").await;
    let _ = sock.flush().await;
}

async fn server_task(listener: tokio::net::TcpListener, streams: StreamMap, tg: Tg) {
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .user_agent("Mozilla/5.0 (Linux; Android 13) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Mobile Safari/537.36")
        .build()
        .unwrap_or_default();
    loop {
        match listener.accept().await {
            Ok((sock, _)) => {
                tokio::spawn(serve_conn(sock, streams.clone(), http.clone(), tg.clone()));
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(200)).await,
        }
    }
}

fn extract_tunnel(line: &str) -> Option<String> {
    let i = line.find("https://")?;
    let rest = &line[i..];
    let suffix = ".trycloudflare.com";
    let end = rest.find(suffix)? + suffix.len();
    let url = rest[..end].to_string();
    if url.contains("://api.") {
        None
    } else {
        Some(url)
    }
}

/// Keeps a free cloudflared "quick tunnel" alive and publishes its https URL in `base`.
async fn tunnel_task(base: Arc<Mutex<Option<String>>>, port: u16) {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let bin = std::env::var("CLOUDFLARED_BIN").unwrap_or_else(|_| {
        let name = if cfg!(windows) {
            "cloudflared.exe"
        } else {
            "cloudflared"
        };
        std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(|d| d.join(name)))
            .filter(|p| p.exists())
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| "cloudflared".to_string())
    });
    loop {
        let spawned = tokio::process::Command::new(&bin)
            .args([
                "tunnel",
                "--no-autoupdate",
                "--url",
                &format!("http://127.0.0.1:{port}"),
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn();
        let mut child = match spawned {
            Ok(c) => c,
            Err(e) => {
                eprintln!("cloudflared not available: {e}");
                tokio::time::sleep(Duration::from_secs(60)).await;
                continue;
            }
        };
        let Some(stderr) = child.stderr.take() else {
            tokio::time::sleep(Duration::from_secs(10)).await;
            continue;
        };
        let mut lines = BufReader::new(stderr).lines();
        let found = tokio::time::timeout(Duration::from_secs(45), async {
            while let Ok(Some(line)) = lines.next_line().await {
                if let Some(u) = extract_tunnel(&line) {
                    return Some(u);
                }
            }
            None
        })
        .await
        .ok()
        .flatten();
        if let Some(u) = found {
            println!("Tunnel ready");
            if let Ok(mut g) = base.lock() {
                *g = Some(u);
            }
            // drain logs until the tunnel process ends
            while let Ok(Some(_)) = lines.next_line().await {}
        }
        let _ = child.kill().await;
        if let Ok(mut g) = base.lock() {
            *g = None;
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

// ------------------------------------------------------------------- Main

fn config_path() -> Option<std::path::PathBuf> {
    dirs::config_dir().map(|d| d.join("moviebox-tgbot").join("config.txt"))
}

/// config.txt lines: token=...  allow=id1,id2
fn read_config(key: &str) -> Option<String> {
    let text = std::fs::read_to_string(config_path()?).ok()?;
    text.lines()
        .filter_map(|l| l.split_once('='))
        .find(|(k, _)| k.trim() == key)
        .map(|(_, v)| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn write_config(token: &str, allow: &str) {
    if let Some(p) = config_path() {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&p, format!("token={token}\nallow={allow}\n"));
    }
}

/// On Render the bot pings its own public address every few minutes, so the
/// free service is never idle (it falls asleep after 15 minutes without traffic).
fn spawn_self_ping(public_url: String) {
    tokio::spawn(async move {
        let url = format!("{public_url}/health");
        loop {
            tokio::time::sleep(Duration::from_secs(240)).await;
            let r = tokio::time::timeout(Duration::from_secs(30), probe_client().get(&url).send())
                .await;
            if !matches!(r, Ok(Ok(_))) {
                eprintln!("[self-ping] no answer from {public_url}");
            }
        }
    });
}

fn restart_text(text: &str) -> String {
    format!(
        "{text}\n\n♻️ <b>The bot was restarted.</b> This link is no longer active. Tap a quality again (search again if the list is gone)."
    )
}

/// Render stops the bot (SIGTERM) on every deploy and restart, and all links
/// live in memory. Before exiting, tell the users whose messages still show a
/// running countdown, so they do not tap a dead link.
#[cfg(unix)]
fn spawn_shutdown_notice(streams: StreamMap, tg: Tg) {
    tokio::spawn(async move {
        use futures::StreamExt;
        use tokio::signal::unix::{SignalKind, signal};
        let Ok(mut term) = signal(SignalKind::terminate()) else {
            return;
        };
        term.recv().await;
        eprintln!("[shutdown] stopping: telling users their links ended");
        let jobs: Vec<(i64, i64, String)> = streams
            .lock()
            .map(|g| {
                g.values()
                    .filter(|s| s.msg != 0)
                    .map(|s| (s.chat, s.msg, s.text.clone()))
                    .collect()
            })
            .unwrap_or_default();
        let work = futures::stream::iter(jobs)
            .map(|(chat, msg, text)| {
                let tg = tg.clone();
                async move {
                    let _ = tg
                        .edit(chat, msg, &restart_text(&text), Some(kb(Vec::new())))
                        .await;
                }
            })
            .buffer_unordered(8)
            .collect::<Vec<_>>();
        let _ = tokio::time::timeout(Duration::from_secs(20), work).await;
        std::process::exit(0);
    });
}

#[cfg(not(unix))]
fn spawn_shutdown_notice(_streams: StreamMap, _tg: Tg) {}

/// Free hosts (Render) fall asleep after a few idle minutes and then need up to
/// a minute to wake up. Ping the sleepy built-in addons now and every 10 minutes.
fn spawn_keep_warm() {
    let urls: Vec<&'static str> = moviebox_tui::providers::addons::BUILTIN_ADDON_MANIFESTS
        .iter()
        .map(|(_, url)| *url)
        .filter(|url| moviebox_tui::providers::addons::is_slow_host(url))
        .collect();
    if urls.is_empty() {
        return;
    }
    tokio::spawn(async move {
        loop {
            for url in &urls {
                let r =
                    tokio::time::timeout(Duration::from_secs(90), probe_client().get(*url).send())
                        .await;
                eprintln!(
                    "[warm] {} -> {}",
                    host_of(url),
                    if matches!(r, Ok(Ok(_))) {
                        "awake"
                    } else {
                        "no answer"
                    }
                );
            }
            tokio::time::sleep(Duration::from_secs(600)).await;
        }
    });
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut arg_token: Option<String> = None;
    let mut arg_allow: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--token" if i + 1 < args.len() => {
                arg_token = Some(args[i + 1].clone());
                i += 1;
            }
            "--allow" if i + 1 < args.len() => {
                arg_allow = Some(args[i + 1].clone());
                i += 1;
            }
            "--help" | "-h" => {
                println!(
                    "moviebox-tgbot [--token BOT_TOKEN] [--allow CHAT_ID,CHAT_ID]\n\nThe token and allowed ids are saved, so next time just run: moviebox-tgbot"
                );
                return;
            }
            _ => {}
        }
        i += 1;
    }

    let env_token = std::env::var("BOT_TOKEN")
        .ok()
        .filter(|t| !t.trim().is_empty());
    let mut token = arg_token.or(env_token).or_else(|| read_config("token"));
    if token.is_none() {
        use std::io::{BufRead, IsTerminal, Write};
        if std::io::stdin().is_terminal() {
            print!("Paste your Telegram bot token (from @BotFather): ");
            let _ = std::io::stdout().flush();
            let mut line = String::new();
            let _ = std::io::stdin().lock().read_line(&mut line);
            let t = line.trim().to_string();
            if !t.is_empty() {
                token = Some(t);
            }
        }
    }
    let token = match token {
        Some(t) => t.trim().to_string(),
        None => {
            eprintln!("BOT_TOKEN is not set (use --token or the BOT_TOKEN variable)");
            std::process::exit(1);
        }
    };
    let allow_raw = arg_allow
        .or_else(|| {
            std::env::var("ALLOWED_CHAT_IDS")
                .ok()
                .filter(|v| !v.trim().is_empty())
        })
        .or_else(|| read_config("allow"))
        .unwrap_or_default();
    let on_github = std::env::var("GITHUB_ACTIONS").is_ok();
    // Hosted (Render): the public https address of this service. PUBLIC_URL can
    // be set by hand; Render provides RENDER_EXTERNAL_URL itself.
    let public_url: Option<String> = std::env::var("PUBLIC_URL")
        .ok()
        .or_else(|| std::env::var("RENDER_EXTERNAL_URL").ok())
        .map(|u| u.trim().trim_end_matches('/').to_string())
        .filter(|u| u.starts_with("http"));
    let hosted = public_url.is_some();
    if !on_github && !hosted {
        write_config(&token, &allow_raw);
    }
    let allowed: Vec<i64> = allow_raw
        .split(',')
        .filter_map(|s| s.trim().parse::<i64>().ok())
        .collect();
    // On GitHub the run window is limited; on your own PC/phone it runs until you stop it.
    let run_minutes: Option<u64> = std::env::var("RUN_MINUTES")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .or(if on_github { Some(345) } else { None });

    let ctx = Ctx {
        tg: Tg::new(&token),
        svc: MovieBoxService::new(),
        states: Arc::new(Mutex::new(HashMap::new())),
        allowed: Arc::new(allowed),
        base: Arc::new(Mutex::new(None)),
        streams: Arc::new(Mutex::new(HashMap::new())),
    };

    let bind_addr = if hosted {
        let port = std::env::var("PORT").unwrap_or_else(|_| "10000".to_string());
        format!("0.0.0.0:{port}")
    } else {
        "127.0.0.1:0".to_string()
    };
    match tokio::net::TcpListener::bind(&bind_addr).await {
        Ok(listener) => {
            let port = listener.local_addr().map(|a| a.port()).unwrap_or(8787);
            spawn_keep_warm();
            spawn_shutdown_notice(ctx.streams.clone(), ctx.tg.clone());
            tokio::spawn(server_task(listener, ctx.streams.clone(), ctx.tg.clone()));
            match public_url.clone() {
                Some(url) => {
                    // Hosted: the service already has a public https address.
                    if let Ok(mut g) = ctx.base.lock() {
                        *g = Some(url.clone());
                    }
                    println!("Public address: {url}");
                    spawn_self_ping(url);
                }
                None => {
                    tokio::spawn(tunnel_task(ctx.base.clone(), port));
                }
            }
        }
        Err(e) => eprintln!("player server failed to start: {e}"),
    }

    let _ = ctx
        .tg
        .call("deleteWebhook", &json!({"drop_pending_updates": false}))
        .await;
    let _ = ctx
        .tg
        .call(
            "setMyCommands",
            &json!({"commands": [
                {"command": "search", "description": "Search all providers"},
                {"command": "help", "description": "How to use the bot"},
                {"command": "id", "description": "Show your chat id"}
            ]}),
        )
        .await;

    println!(
        "Bot started ({})",
        if on_github {
            "GitHub Actions"
        } else if hosted {
            "hosted"
        } else {
            "local"
        }
    );
    let deadline: Option<Instant> =
        run_minutes.map(|m| Instant::now() + Duration::from_secs(m * 60));
    let mut offset: i64 = 0;

    while deadline.is_none_or(|d| Instant::now() < d) {
        let body = json!({
            "offset": offset,
            "timeout": 30,
            "allowed_updates": ["message", "callback_query"]
        });
        match ctx.tg.call("getUpdates", &body).await {
            Ok(v) => {
                if let Some(arr) = v.as_array() {
                    for u in arr {
                        if let Some(id) = u.get("update_id").and_then(Value::as_i64) {
                            offset = id + 1;
                        }
                        tokio::spawn(handle_update(ctx.clone(), u.clone()));
                    }
                }
            }
            Err(e) => {
                eprintln!("getUpdates error: {e}");
                if e.contains("Conflict") {
                    // Another instance (GitHub vs your PC/phone) is polling the same bot.
                    // The GitHub copy steps aside so your local BDIX-capable copy wins.
                    let wait = if on_github { 300 } else { 2 };
                    tokio::time::sleep(Duration::from_secs(wait)).await;
                } else {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    }

    // Give in-flight handlers a moment, then acknowledge processed updates
    // so the next run does not repeat them.
    tokio::time::sleep(Duration::from_secs(10)).await;
    let _ = ctx
        .tg
        .call("getUpdates", &json!({"offset": offset, "timeout": 0}))
        .await;
    println!("Run window finished; exiting");
}

#[cfg(test)]
mod tests {
    use super::*;
    use moviebox_tui::providers::models::ProviderMediaId;

    fn item(p: ProviderKind, id: &str, title: &str, year: Option<&str>) -> CatalogItem {
        CatalogItem {
            id: ProviderMediaId {
                provider: p,
                value: id.to_string(),
            },
            title: title.to_string(),
            media_type: MediaType::Movie,
            year: year.map(str::to_string),
            poster_url: None,
            season_count: None,
        }
    }

    #[test]
    fn same_name_and_year_merge_across_providers() {
        let results = vec![
            (
                ProviderKind::MovieBox,
                vec![
                    item(ProviderKind::MovieBox, "m1", "Alpha", Some("2025")),
                    item(ProviderKind::MovieBox, "m2", "Alpha", Some("2013")),
                ],
            ),
            (
                ProviderKind::Dramachi,
                vec![item(
                    ProviderKind::Dramachi,
                    "d1",
                    "Alpha (2025)",
                    Some("2025-03-01"),
                )],
            ),
            (
                ProviderKind::Addons,
                vec![item(
                    ProviderKind::Addons,
                    "movie:tt1",
                    "The Alpha",
                    Some("2025"),
                )],
            ),
        ];
        let groups = build_groups(&results);
        assert_eq!(groups.len(), 2, "Alpha 2025 and Alpha 2013 stay separate");
        // the title found on the most providers comes first
        assert_eq!(groups[0].year, "2025");
        assert_eq!(distinct_providers(&groups[0].members), 3);
        assert_eq!(groups[1].year, "2013");
        assert_eq!(groups[1].members.len(), 1);
    }

    #[test]
    fn titles_and_years_are_normalised() {
        assert_eq!(norm_title("The Avengers [Hindi]"), "avengers");
        assert_eq!(norm_title("Tom & Jerry"), "tom and jerry");
        assert_eq!(year_of(Some("Released 2012-05-04")), "2012");
        assert_eq!(year_of(Some("n/a")), "");
        assert_eq!(clean_title("Alpha [Hindi] (2025)"), "Alpha");
    }

    #[test]
    fn download_link_and_file_name() {
        assert_eq!(
            dl_url("https://x.trycloudflare.com/p/abc123", 2),
            "https://x.trycloudflare.com/d/abc123?n=2"
        );
        assert_eq!(
            file_ext("https://h/a/movie.mkv?token=1", "video/mp4"),
            "mkv"
        );
        assert_eq!(file_ext("https://h/stream", "video/x-matroska"), "mkv");
        assert_eq!(file_ext("https://h/stream", ""), "mp4");
        assert_eq!(
            download_name("Alpha (2025) 2160p", "mkv"),
            "Alpha (2025) 2160p.mkv"
        );
        assert_eq!(download_name("___", "mp4"), "video.mp4");
    }

    async fn serve_once(
        status_line: &'static str,
        headers: &'static str,
        body: &'static [u8],
    ) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for _ in 0..4 {
                if let Ok((mut sock, _)) = listener.accept().await {
                    let mut buf = [0u8; 2048];
                    let _ = sock.read(&mut buf).await;
                    let head =
                        format!("HTTP/1.1 {status_line}\r\n{headers}connection: close\r\n\r\n");
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.write_all(body).await;
                    let _ = sock.shutdown().await;
                }
            }
        });
        format!("http://{addr}/file.mkv")
    }

    #[tokio::test]
    async fn links_need_http_200_and_more_than_50mb() {
        let http = reqwest::Client::new();

        // passes: 206 and a 5 GB file
        let big = serve_once(
            "206 Partial Content",
            "content-type: video/x-matroska\r\ncontent-range: bytes 0-1/5000000000\r\ncontent-length: 2\r\n",
            b"ab",
        )
        .await;
        assert!(link_status(&http, &big, &[]).await.is_ok());

        // passes: host ignores Range, answers 200 with a 700 MB length
        let full = serve_once(
            "200 OK",
            "content-type: video/mp4\r\ncontent-length: 734003200\r\n",
            b"x",
        )
        .await;
        assert!(link_status(&http, &full, &[]).await.is_ok());

        // fails test 1: not found
        let missing = serve_once(
            "404 Not Found",
            "content-type: text/plain\r\ncontent-length: 0\r\n",
            b"",
        )
        .await;
        assert_eq!(
            link_status(&http, &missing, &[]).await,
            Err("HTTP 404".to_string())
        );

        // fails test 2: only 20 MB
        let small = serve_once(
            "206 Partial Content",
            "content-range: bytes 0-1/20971520\r\ncontent-length: 2\r\n",
            b"ab",
        )
        .await;
        assert!(link_status(&http, &small, &[]).await.is_err());

        // fails test 2: size unknown
        let unknown = serve_once("200 OK", "content-type: video/mp4\r\n", b"x").await;
        assert!(link_status(&http, &unknown, &[]).await.is_err());
    }

    #[test]
    fn countdown_is_shown_as_hours_minutes_seconds() {
        assert_eq!(fmt_hms(UNLOCK_SECS), "06:00:00");
        assert_eq!(fmt_hms(7325), "02:02:05");
        assert_eq!(fmt_hms(59), "00:00:59");
        assert_eq!(fmt_hms(0), "00:00:00");
        assert!(unlocked_text("x", 3661).contains("01:01:01"));
    }

    #[test]
    fn release_years_one_apart_still_match() {
        assert!(years_close("2018", "2019"));
        assert!(years_close("2018", "2018"));
        assert!(!years_close("2018", "2020"));
        assert!(!years_close("", "2018"));
        let results = vec![
            (
                ProviderKind::MovieBox,
                vec![item(
                    ProviderKind::MovieBox,
                    "m1",
                    "Bachelor Point",
                    Some("2018"),
                )],
            ),
            (
                ProviderKind::Addons,
                vec![item(
                    ProviderKind::Addons,
                    "series:tt1",
                    "Bachelor Point",
                    Some("2019"),
                )],
            ),
        ];
        let groups = build_groups(&results);
        assert_eq!(groups.len(), 1);
        assert_eq!(distinct_providers(&groups[0].members), 2);
    }

    #[test]
    fn per_season_titles_are_split_and_grouped() {
        assert_eq!(
            split_season("Bachelor Point S2"),
            ("Bachelor Point".to_string(), Some(2))
        );
        assert_eq!(
            split_season("Bachelor Point [Bengali] Season 3"),
            ("Bachelor Point".to_string(), Some(3))
        );
        assert_eq!(split_season("Iron Man 2"), ("Iron Man 2".to_string(), None));
        assert_eq!(split_season("Se7en"), ("Se7en".to_string(), None));
        assert_eq!(split_season("S1"), ("S1".to_string(), None));

        let mut s1 = item(
            ProviderKind::MovieBox,
            "1",
            "Bachelor Point S1",
            Some("2018"),
        );
        let mut s2 = item(
            ProviderKind::MovieBox,
            "2",
            "Bachelor Point S2",
            Some("2019"),
        );
        let mut s5 = item(
            ProviderKind::MovieBox,
            "5",
            "Bachelor Point S5",
            Some("2025"),
        );
        let mut cine = item(
            ProviderKind::Addons,
            "series:tt1",
            "Bachelor Point",
            Some("2018"),
        );
        for it in [&mut s1, &mut s2, &mut s5, &mut cine] {
            it.media_type = MediaType::Series;
        }
        let results = vec![
            (ProviderKind::MovieBox, vec![s5, s1, s2]),
            (ProviderKind::Addons, vec![cine]),
        ];
        let groups = build_groups(&results);
        assert_eq!(groups.len(), 1, "all seasons end up in one title");
        assert_eq!(groups[0].title, "Bachelor Point");
        assert_eq!(groups[0].year, "2018");
        assert_eq!(groups[0].members.len(), 4);
    }

    #[tokio::test]
    async fn hls_playlists_pass_when_valid() {
        let http = reqwest::Client::new();
        let good = serve_once(
            "200 OK",
            "content-type: application/vnd.apple.mpegurl\r\n",
            b"#EXTM3U\n#EXT-X-VERSION:3\n",
        )
        .await;
        assert!(link_status(&http, &good, &[]).await.is_ok());
        let bad = serve_once(
            "200 OK",
            "content-type: application/vnd.apple.mpegurl\r\n",
            b"<html>oops</html>",
        )
        .await;
        assert!(link_status(&http, &bad, &[]).await.is_err());
        assert!(is_hls("https://cdn.example.com/x/master.m3u8?token=1"));
        assert!(!is_hls("https://cdn.example.com/x/movie.mkv"));
    }

    #[test]
    fn hls_playlists_point_back_to_this_server() {
        let base = url::Url::parse("https://cdn.example.com/a/b/index.m3u8?x=1").unwrap();
        let text = "#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXT-X-KEY:METHOD=AES-128,URI=\"https://k.example.com/key\"\n#EXTINF:4,\nseg1.ts\n/abs/seg2.ts\n";
        let out = rewrite_playlist(text, &base, "abc", 2);
        assert!(out.starts_with("#EXTM3U\n"));
        assert!(out.lines().all(|l| !l.contains("cdn.example.com")));
        assert_eq!(out.matches("/h/abc?n=2&u=").count(), 4);
        // the original address can be recovered
        use base64::Engine;
        let enc = out
            .lines()
            .find(|l| l.starts_with("/h/abc?n=2&u="))
            .unwrap()
            .trim_start_matches("/h/abc?n=2&u=");
        let back = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(enc)
            .unwrap();
        assert_eq!(
            String::from_utf8(back).unwrap(),
            "https://cdn.example.com/a/b/seg1.ts"
        );
        let (map, segs, enc_on) = parse_media_playlist(text, &base);
        assert_eq!(map.as_deref(), Some("https://cdn.example.com/a/b/init.mp4"));
        assert_eq!(segs.len(), 2);
        assert!(enc_on);
    }

    /// A tiny HLS "CDN": a playlist (relative pieces) and two pieces.
    async fn spawn_fake_cdn() -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    continue;
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let path = req.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let (ct, body): (&str, &[u8]) = match path.as_str() {
                        "/v/index.m3u8" => (
                            "application/vnd.apple.mpegurl",
                            b"#EXTM3U\n#EXT-X-VERSION:3\n#EXTINF:4,\nseg0.ts\n#EXTINF:4,\nseg1.ts\n#EXT-X-ENDLIST\n",
                        ),
                        "/v/seg0.ts" => ("video/mp2t", b"AAAA"),
                        "/v/seg1.ts" => ("video/mp2t", b"BBBB"),
                        _ => ("text/plain", b"nope"),
                    };
                    let head = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: {ct}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.write_all(body).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        format!("http://{addr}/v/index.m3u8")
    }

    #[tokio::test]
    async fn hls_stream_plays_and_downloads_through_the_bot_server() {
        let cdn_playlist = spawn_fake_cdn().await;
        let http = reqwest::Client::new();

        // the bot's player server
        let streams: StreamMap = Arc::new(Mutex::new(HashMap::new()));
        streams.lock().unwrap().insert(
            "abc".to_string(),
            Stream {
                items: vec![StreamItem {
                    name: "HLS".to_string(),
                    url: cdn_playlist,
                    headers: Vec::new(),
                    hls: true,
                }],
                title: "Show".to_string(),
                file_base: "Show S01E01 1080p".to_string(),
                chat: 0,
                msg: 0,
                text: String::new(),
                page: "http://x/p/abc".to_string(),
                unlocked_at: Some(Instant::now()),
            },
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let bot = listener.local_addr().unwrap();
        tokio::spawn(server_task(listener, streams, Tg::new("test")));

        // play: the playlist now points to the bot server
        let pl = http
            .get(format!("http://{bot}/s/abc?n=0"))
            .send()
            .await
            .unwrap();
        assert_eq!(pl.status(), 200);
        let pl_text = pl.text().await.unwrap();
        assert!(pl_text.starts_with("#EXTM3U"));
        assert!(!pl_text.contains("127.0.0.1"));
        let first_piece = pl_text
            .lines()
            .find(|l| l.starts_with("/h/abc?n=0&u="))
            .expect("rewritten piece address");
        let piece = http
            .get(format!("http://{bot}{first_piece}"))
            .send()
            .await
            .unwrap();
        assert_eq!(piece.bytes().await.unwrap().as_ref(), b"AAAA");

        // download: all pieces joined into one file
        let dl = http
            .get(format!("http://{bot}/d/abc?n=0"))
            .send()
            .await
            .unwrap();
        assert_eq!(dl.status(), 200);
        let disp = dl
            .headers()
            .get("content-disposition")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(disp.contains("Show S01E01 1080p.ts"), "{disp}");
        assert_eq!(dl.bytes().await.unwrap().as_ref(), b"AAAABBBB");
    }

    #[tokio::test]
    async fn health_endpoint_answers_for_uptime_monitors() {
        let streams: StreamMap = Arc::new(Mutex::new(HashMap::new()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let bot = listener.local_addr().unwrap();
        tokio::spawn(server_task(listener, streams, Tg::new("test")));
        let http = reqwest::Client::new();
        for path in ["/health", "/"] {
            let r = http
                .get(format!("http://{bot}{path}"))
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 200);
            assert_eq!(r.text().await.unwrap(), "ok");
        }
        let head = http
            .head(format!("http://{bot}/health"))
            .send()
            .await
            .unwrap();
        assert_eq!(head.status(), 200);
        // unknown stream ids are still refused
        let missing = http
            .get(format!("http://{bot}/s/doesnotexist?n=0"))
            .send()
            .await
            .unwrap();
        assert_ne!(missing.status(), 200);
    }

    fn release(
        p: ProviderKind,
        q: &str,
        lang: Option<&str>,
        codec: &str,
        label: &str,
        size: u64,
    ) -> Release {
        use moviebox_tui::providers::models::SourceMirror;
        Release {
            provider: p,
            filename: String::new(),
            quality: Some(q.to_string()),
            codec: Some(codec.to_string()),
            language: lang.map(str::to_string),
            size_bytes: Some(size),
            season: None,
            episode: None,
            mirrors: vec![SourceMirror {
                label: label.to_string(),
                resolver_url: "https://x/y".to_string(),
                headers: Vec::new(),
                direct_file: true,
            }],
            resource_id: None,
        }
    }

    #[test]
    fn labels_show_audio_language_and_provider() {
        let gb = 1_073_741_824u64;
        let mb = release(
            ProviderKind::MovieBox,
            "1080p",
            Some("Hindi"),
            "h264",
            "Promise",
            3 * gb,
        );
        assert_eq!(rel_short(&mb), "1080p • 3.00 GB • Hindi • MovieBox");
        assert_eq!(
            rel_full(&mb),
            "1080p • 3.00 GB • Hindi • h264 • MovieBox · Promise"
        );

        let ad = release(
            ProviderKind::Addons,
            "1080p",
            Some("Bengali"),
            "AVC/x264",
            "BanglaPlex · 1080p Bachelor Point",
            2 * gb,
        );
        assert_eq!(provider_name(&ad), "BanglaPlex");
        assert_eq!(rel_short(&ad), "1080p • 2.00 GB • Bengali • BanglaPlex");
        assert_eq!(
            rel_full(&ad),
            "1080p • 2.00 GB • Bengali • AVC/x264 • BanglaPlex · 1080p Bachelor Point"
        );

        // no language known: nothing invented
        let k = release(
            ProviderKind::FourKHdHub,
            "2160p",
            None,
            "HEVC",
            "4KHDHub",
            gb,
        );
        assert_eq!(rel_short(&k), "2160p • 1.00 GB • 4KHDHub");
    }

    #[test]
    fn year_in_the_title_does_not_split_a_movie() {
        assert_eq!(
            strip_trailing_year("Avengers Endgame 2019", "2019"),
            "Avengers Endgame"
        );
        assert_eq!(
            strip_trailing_year("Blade Runner 2049", "2017"),
            "Blade Runner 2049"
        );
        assert_eq!(strip_trailing_year("2019", "2019"), "2019");
        let results = vec![
            (
                ProviderKind::Dramachi,
                vec![item(
                    ProviderKind::Dramachi,
                    "d",
                    "Avengers Endgame 2019",
                    Some("2019"),
                )],
            ),
            (
                ProviderKind::Addons,
                vec![item(
                    ProviderKind::Addons,
                    "movie:tt4154796",
                    "Avengers: Endgame",
                    Some("2019"),
                )],
            ),
        ];
        let groups = build_groups(&results);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].title, "Avengers Endgame");
        assert_eq!(distinct_providers(&groups[0].members), 2);
    }

    #[test]
    fn sources_line_explains_every_provider() {
        let mb = Summary {
            found: 14,
            ..Default::default()
        };
        let k = Summary {
            why: Some("no streams found".to_string()),
            ..Default::default()
        };
        let ad = Summary {
            found: 3,
            hidden: 3,
            hidden_why: Some("HTTP 403".to_string()),
            ..Default::default()
        };
        let summary = vec![
            (ProviderKind::MovieBox, mb),
            (ProviderKind::FourKHdHub, k),
            (ProviderKind::Addons, ad),
        ];
        let note = sources_note(
            &summary,
            &|p| if p == ProviderKind::MovieBox { 14 } else { 0 },
            &["Dramachi"],
        );
        assert!(note.contains("MovieBox 14"));
        assert!(note.contains("4KHDHub 0 (no streams found)"));
        assert!(note.contains("Addons 0 (3 link(s) hidden: HTTP 403)"));
        assert!(note.contains("Not found on: Dramachi"));
    }

    #[test]
    fn results_are_listed_newest_first() {
        let mk = |p, id: &str, title: &str, year: Option<&str>| item(p, id, title, year);
        let results = vec![(
            ProviderKind::MovieBox,
            vec![
                mk(ProviderKind::MovieBox, "1", "Alpha", Some("2012")),
                mk(ProviderKind::MovieBox, "2", "Beta", Some("2026")),
                mk(ProviderKind::MovieBox, "3", "Gamma", None),
                mk(ProviderKind::MovieBox, "4", "Delta", Some("2019")),
            ],
        )];
        let years: Vec<String> = build_groups(&results).into_iter().map(|g| g.year).collect();
        // newest first; a title without a year goes last
        assert_eq!(years, vec!["2026", "2019", "2012", ""]);
    }

    #[test]
    fn one_ad_is_enough() {
        assert_eq!(ADS_REQUIRED, 1);
        assert_eq!(ads_word(1), "1 ad");
        assert_eq!(ads_word(2), "2 ads");
        let page = gate_page("abc", "Show");
        assert!(page.contains("N=1"));
        assert!(page.contains("Watch 1 ad to unlock"));
        assert!(!page.contains("__ADS"));
    }

    #[test]
    fn restart_notice_replaces_the_countdown() {
        let t = restart_text("Alpha (2025)\n1080p");
        assert!(t.starts_with("Alpha (2025)"));
        assert!(t.contains("The bot was restarted"));
        assert!(!t.contains("Time left"));
        assert!(String::from_utf8_lossy(LINK_GONE_TEXT).contains("tap the quality again"));
    }

    #[test]
    fn download_row_has_a_copy_link_button() {
        let st = Stream {
            items: vec![StreamItem {
                name: "HdHub".to_string(),
                url: "https://files.example.com/a.mkv".to_string(),
                headers: Vec::new(),
                hls: false,
            }],
            title: "Show".to_string(),
            file_base: "Show".to_string(),
            chat: 1,
            msg: 2,
            text: String::new(),
            page: "https://bot.onrender.com/p/0123456789abcdef0123456789abcdef".to_string(),
            unlocked_at: Some(Instant::now()),
        };
        let with = play_keyboard(&st, true, true).to_string();
        let expect = "https://bot.onrender.com/d/0123456789abcdef0123456789abcdef?n=0";
        assert!(with.contains(&format!(r#""copy_text":{{"text":"{expect}"}}"#)));
        assert!(
            expect.len() <= 256,
            "Telegram allows 256 characters to copy"
        );
        // the plain Download button is still there
        assert!(with.contains("Download"));
        // the fallback keyboard has no copy button
        let without = play_keyboard(&st, false, false).to_string();
        assert!(!without.contains("copy_text"));
        assert!(without.contains(expect));
    }
}
