//! Tiny Atom/RSS feed reader that sends new items to Telegram.

use std::collections::BTreeMap;
use std::error::Error;
use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::Duration;
use std::{env, process, thread};

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use ureq::tls::{RootCerts, TlsConfig};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

const USAGE: &str = "\
Tiny Atom/RSS feed reader that sends new items to Telegram.

Usage:
  local-rss telegram [chat_id]
                          connect Telegram; without chat_id, uses the last chat
                          that messaged the bot (send it any message first)
  local-rss test          send a test message to Telegram
  local-rss add <url>     subscribe to a feed (existing items are marked as seen)
  local-rss remove <feed> unsubscribe
  local-rss list          show subscribed feeds (numbered) and their filters
  local-rss filter <feed> include <regex>
                          only send items whose title matches (any include)
  local-rss filter <feed> exclude <regex>
                          never send items whose title matches
  local-rss filter <feed> clear
                          remove all filters
  local-rss preview <feed>
                          show current feed items and which would be sent
  local-rss check         check all feeds once, send new items to Telegram
  local-rss run [minutes] check in a loop (default every 10 minutes)

<feed> is a number from `list` or the feed URL. Filters match the item title,
e.g. for GitLab: 'pushed new project tag', 'pushed to project branch (main|master) '.

Config (environment variables, or a .env file in the data directory):
  TELEGRAM_TOKEN          bot token from @BotFather (required)
  TELEGRAM_CHAT_ID        chat to send to (optional; `local-rss telegram` saves it)
  TELEGRAM_THREAD_ID      forum topic to post in (optional; for groups with topics)
  LOCAL_RSS_DIR           where data.json and .env live (default: the project dir)";

const ATOM_NS: &str = "http://www.w3.org/2005/Atom";
const MAX_SEEN_PER_FEED: usize = 500;

// Platform verifier uses the macOS keychain, same trust as Safari/curl.
static AGENT: LazyLock<ureq::Agent> = LazyLock::new(|| {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(30)))
        .http_status_as_error(false)
        .user_agent("local-rss/1.0")
        .tls_config(TlsConfig::builder().root_certs(RootCerts::PlatformVerifier).build())
        .build()
        .into()
});

#[derive(Default, Serialize, Deserialize)]
struct Data {
    #[serde(default)]
    feeds: BTreeMap<String, Feed>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    telegram: Option<Telegram>,
}

#[derive(Default, Serialize, Deserialize)]
struct Feed {
    #[serde(default)]
    title: String,
    #[serde(default)]
    seen: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    include: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    exclude: Vec<String>,
}

struct Filter {
    include: Vec<Regex>,
    exclude: Vec<Regex>,
}

impl Filter {
    fn new(feed: &Feed) -> Result<Self> {
        let compile = |patterns: &[String]| -> Result<Vec<Regex>> {
            patterns.iter().map(|p| Regex::new(p).map_err(|e| format!("bad filter {p:?}: {e}").into())).collect()
        };
        Ok(Filter { include: compile(&feed.include)?, exclude: compile(&feed.exclude)? })
    }

    fn matches(&self, title: &str) -> bool {
        (self.include.is_empty() || self.include.iter().any(|r| r.is_match(title)))
            && !self.exclude.iter().any(|r| r.is_match(title))
    }
}

/// Accept either a 1-based number from `list` or the feed URL.
fn resolve_feed(data: &Data, key: &str) -> Result<String> {
    if data.feeds.contains_key(key) {
        return Ok(key.to_string());
    }
    key.parse::<usize>()
        .ok()
        .and_then(|n| n.checked_sub(1))
        .and_then(|i| data.feeds.keys().nth(i))
        .cloned()
        .ok_or_else(|| format!("no such feed: {key} (see `local-rss list`)").into())
}

#[derive(Serialize, Deserialize)]
struct Telegram {
    chat_id: Value,
}

struct Item {
    id: String,
    title: String,
    link: String,
}

fn data_dir() -> PathBuf {
    env::var_os("LOCAL_RSS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")))
}

fn load() -> Result<Data> {
    let path = data_dir().join("data.json");
    if !path.exists() {
        return Ok(Data::default());
    }
    Ok(serde_json::from_str(&fs::read_to_string(path)?)?)
}

fn save(data: &Data) -> Result<()> {
    let path = data_dir().join("data.json");
    let tmp = path.with_extension("tmp");
    // Feed URLs can contain tokens, so keep the file private.
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(serde_json::to_string_pretty(data)?.as_bytes())?;
    fs::rename(tmp, path)?;
    Ok(())
}

fn fetch(url: &str) -> Result<String> {
    let mut resp = AGENT.get(url).call()?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("HTTP {status}").into());
    }
    let body = String::from_utf8_lossy(&resp.body_mut().read_to_vec()?).into_owned();
    let head = body.trim_start().get(..20).unwrap_or("").to_ascii_lowercase();
    if head.starts_with("<!doctype html") || head.starts_with("<html") {
        return Err("got an HTML page instead of a feed (often a login redirect: \
                    check the URL and that any feed_token is still valid)"
            .into());
    }
    Ok(body)
}

fn child<'a, 'i>(
    node: roxmltree::Node<'a, 'i>,
    ns: Option<&str>,
    name: &str,
) -> Option<roxmltree::Node<'a, 'i>> {
    node.children()
        .find(|c| c.is_element() && c.tag_name().name() == name && c.tag_name().namespace() == ns)
}

fn child_text(node: roxmltree::Node, ns: Option<&str>, name: &str) -> String {
    child(node, ns, name)
        .and_then(|c| c.text())
        .unwrap_or("")
        .trim()
        .to_string()
}

/// Some feeds (GitLab) escape titles twice, leaving `&quot;` etc. in the text.
fn unescape_entities(s: &str) -> String {
    s.replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn or_untitled(title: String) -> String {
    if title.is_empty() { "(untitled)".into() } else { unescape_entities(&title) }
}

/// Returns (feed_title, items) for Atom or RSS 2.0.
fn parse(xml: &str) -> Result<(String, Vec<Item>)> {
    let doc = roxmltree::Document::parse(xml)?;
    let root = doc.root_element();
    let atom = Some(ATOM_NS);

    if root.tag_name().name() == "feed" && root.tag_name().namespace() == atom {
        let items = root
            .children()
            .filter(|n| n.is_element() && n.tag_name().name() == "entry" && n.tag_name().namespace() == atom)
            .map(|entry| {
                let link = entry
                    .children()
                    .filter(|l| l.is_element() && l.tag_name().name() == "link")
                    .find(|l| l.attribute("rel").unwrap_or("alternate") == "alternate")
                    .and_then(|l| l.attribute("href"))
                    .unwrap_or("")
                    .to_string();
                let id = child_text(entry, atom, "id");
                Item {
                    id: if id.is_empty() { link.clone() } else { id },
                    title: or_untitled(child_text(entry, atom, "title")),
                    link,
                }
            })
            .collect();
        return Ok((child_text(root, atom, "title"), items));
    }

    if let Some(channel) = child(root, None, "channel") {
        let items = channel
            .children()
            .filter(|n| n.is_element() && n.tag_name().name() == "item")
            .map(|item| {
                let link = child_text(item, None, "link");
                let title = child_text(item, None, "title");
                let id = [child_text(item, None, "guid"), link.clone(), title.clone()]
                    .into_iter()
                    .find(|s| !s.is_empty())
                    .unwrap_or_default();
                Item { id, title: or_untitled(title), link }
            })
            .collect();
        return Ok((child_text(channel, None, "title"), items));
    }

    Err(format!("unknown feed format: <{}>", root.tag_name().name()).into())
}

fn telegram_api(token: &str, method: &str, params: &[(&str, &str)]) -> Result<Value> {
    let url = format!("https://api.telegram.org/bot{token}/{method}");
    let mut resp = AGENT.post(&url).send_form(params.iter().copied())?;
    let body: Value = serde_json::from_str(&resp.body_mut().read_to_string()?)?;
    if body["ok"].as_bool() != Some(true) {
        let desc = body["description"].as_str().unwrap_or("unknown Telegram error");
        return Err(desc.to_string().into());
    }
    Ok(body["result"].clone())
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

struct TgConfig {
    token: Option<String>,
    chat_id: Option<String>,
    thread_id: Option<String>,
}

impl TgConfig {
    fn from(data: &Data) -> Self {
        let non_empty = |k| env::var(k).ok().filter(|v| !v.is_empty());
        let saved = data.telegram.as_ref().map(|t| match &t.chat_id {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        });
        TgConfig {
            token: non_empty("TELEGRAM_TOKEN"),
            chat_id: non_empty("TELEGRAM_CHAT_ID").or(saved),
            thread_id: non_empty("TELEGRAM_THREAD_ID"),
        }
    }

    fn send(&self, feed_title: &str, title: &str, link: &str) -> Result<()> {
        let token = self.token.as_deref().ok_or("TELEGRAM_TOKEN is not set (in the environment or .env)")?;
        let chat_id = self
            .chat_id
            .as_deref()
            .ok_or("no chat id: run `local-rss telegram` or set TELEGRAM_CHAT_ID")?;
        let mut text = format!("<b>{}</b>\n", html_escape(feed_title));
        if link.is_empty() {
            text += &html_escape(title);
        } else {
            text += &format!("<a href=\"{}\">{}</a>", html_escape(link), html_escape(title));
        }
        let mut params = vec![("chat_id", chat_id), ("text", &text), ("parse_mode", "HTML")];
        if let Some(thread_id) = &self.thread_id {
            params.push(("message_thread_id", thread_id));
        }
        telegram_api(token, "sendMessage", &params)?;
        Ok(())
    }
}

fn check(data: &mut Data) -> Result<()> {
    let tg = TgConfig::from(data);
    for (url, feed) in data.feeds.iter_mut() {
        let filter = match Filter::new(feed) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("[error] {url}: {e}");
                continue;
            }
        };
        let (title, items) = match fetch(url).and_then(|xml| parse(&xml)) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[error] {url}: {e}");
                continue;
            }
        };

        if !title.is_empty() {
            feed.title = title;
        } else if feed.title.is_empty() {
            feed.title = url.clone();
        }

        let new: Vec<&Item> = items.iter().filter(|it| !feed.seen.contains(&it.id)).collect();
        for it in new.into_iter().rev() {
            // oldest first
            if !filter.matches(&it.title) {
                // Filtered out: mark as seen so it isn't reconsidered.
                feed.seen.push(it.id.clone());
                continue;
            }
            println!("[new] {}: {}  {}", feed.title, it.title, it.link);
            if let Err(e) = tg.send(&feed.title, &it.title, &it.link) {
                // Leave it unseen so it's retried on the next check.
                eprintln!("[error] telegram: {e}");
                break;
            }
            feed.seen.push(it.id.clone());
        }

        let excess = feed.seen.len().saturating_sub(MAX_SEEN_PER_FEED);
        feed.seen.drain(..excess);
    }
    save(data)
}

fn run(args: &[&str]) -> Result<()> {
    let mut data = load()?;

    match args {
        ["telegram", rest @ ..] if rest.len() <= 1 => {
            let tg = TgConfig::from(&data);
            let token = tg.token.as_deref().ok_or("Set TELEGRAM_TOKEN in .env (or the environment) first.")?;
            let chat_id = match rest {
                [id] => id.to_string(),
                _ => {
                    let updates = telegram_api(token, "getUpdates", &[])
                        .map_err(|e| format!("Telegram setup failed: {e}"))?;
                    let last = updates
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|u| u["message"]["chat"]["id"].as_i64())
                        .last();
                    match last {
                        Some(id) => id.to_string(),
                        None => {
                            return Err("No messages found. Send any message to your bot in Telegram, \
                                        then rerun this."
                                .into());
                        }
                    }
                }
            };
            let tg = TgConfig { chat_id: Some(chat_id.clone()), ..tg };
            tg.send("local-rss", "Telegram connected ✅", "")
                .map_err(|e| format!("Telegram setup failed: {e}"))?;
            let value = chat_id.parse::<i64>().map(Value::from).unwrap_or(Value::from(chat_id.clone()));
            data.telegram = Some(Telegram { chat_id: value });
            save(&data)?;
            println!("Telegram configured (chat_id {chat_id}); sent a confirmation message.");
        }
        ["test"] => {
            TgConfig::from(&data)
                .send("local-rss", "Test message", "https://example.com")
                .map_err(|e| format!("Failed: {e}"))?;
            println!("Sent.");
        }
        ["add", url] => {
            let (title, items) =
                fetch(url).and_then(|xml| parse(&xml)).map_err(|e| format!("Could not add feed: {e}"))?;
            let shown = if title.is_empty() { url.to_string() } else { title.clone() };
            println!("Added '{title}' ({} existing items marked as seen)", items.len());
            data.feeds.insert(
                url.to_string(),
                Feed { title: shown, seen: items.into_iter().map(|it| it.id).collect(), ..Default::default() },
            );
            save(&data)?;
        }
        ["remove", key] => {
            let url = resolve_feed(&data, key)?;
            data.feeds.remove(&url);
            save(&data)?;
            println!("Removed {url}");
        }
        ["list"] => {
            for (i, (url, feed)) in data.feeds.iter().enumerate() {
                println!("{}. {}\n   {url}", i + 1, feed.title);
                for p in &feed.include {
                    println!("   include: {p}");
                }
                for p in &feed.exclude {
                    println!("   exclude: {p}");
                }
            }
        }
        ["filter", key, kind @ ("include" | "exclude"), pattern] => {
            Regex::new(pattern).map_err(|e| format!("bad regex: {e}"))?;
            let url = resolve_feed(&data, key)?;
            let feed = data.feeds.get_mut(&url).unwrap();
            let list = if *kind == "include" { &mut feed.include } else { &mut feed.exclude };
            list.push(pattern.to_string());
            let title = feed.title.clone();
            save(&data)?;
            println!("Added {kind} filter {pattern:?} to '{title}'");
        }
        ["filter", key, "clear"] => {
            let url = resolve_feed(&data, key)?;
            let feed = data.feeds.get_mut(&url).unwrap();
            feed.include.clear();
            feed.exclude.clear();
            let title = feed.title.clone();
            save(&data)?;
            println!("Cleared filters for '{title}'");
        }
        ["preview", key] => {
            let url = resolve_feed(&data, key)?;
            let feed = &data.feeds[&url];
            let filter = Filter::new(feed)?;
            let (_, items) = fetch(&url).and_then(|xml| parse(&xml))?;
            let mut sent = 0;
            for it in &items {
                let ok = filter.matches(&it.title);
                sent += ok as usize;
                println!("{} {}", if ok { "✅" } else { "  ·" }, it.title);
            }
            println!("\n{sent} of {} items would be sent", items.len());
        }
        ["check"] => check(&mut data)?,
        [] | ["run"] | ["run", _] => {
            let minutes: f64 = match args.get(1) {
                Some(m) => m.parse().map_err(|_| format!("invalid minutes: {m}"))?,
                None => 10.0,
            };
            println!("Checking {} feeds every {minutes} min. Ctrl+C to stop.", data.feeds.len());
            loop {
                if let Err(e) = load().and_then(|mut d| check(&mut d)) {
                    eprintln!("[error] {e}");
                }
                thread::sleep(Duration::from_secs_f64(minutes * 60.0));
            }
        }
        _ => {
            println!("{USAGE}");
            process::exit(1);
        }
    }
    Ok(())
}

fn main() {
    // Real environment variables take precedence over .env.
    let _ = dotenvy::from_path(data_dir().join(".env"));
    let args: Vec<String> = env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    if let Err(e) = run(&args) {
        eprintln!("{e}");
        process::exit(1);
    }
}
