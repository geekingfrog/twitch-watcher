#[macro_use]
extern crate anyhow;

use anyhow::{Context, Result};
use chrono::prelude::{DateTime, Utc};
use chrono::Duration;
use directories;
use log;
use notify_rust::Notification;
use reqwest;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::env;
use std::path;
use structopt::StructOpt;

#[derive(Deserialize, Debug)]
struct TokenResponse {
    access_token: String,
    expires_in: u32,
    token_type: String,
}

#[derive(Deserialize, Debug)]
struct TwitchResponseData<T> {
    data: Vec<T>,
}

#[derive(Deserialize, Debug)]
struct StreamResponseData {
    id: String,
    user_id: String,
    user_name: String,
    game_id: String,
    game_name: String,
    viewer_count: u32,
    started_at: DateTime<Utc>,
}

#[derive(Deserialize, Debug)]
struct UserResponseData {
    id: String,
    login: String,
    display_name: String,
}

#[derive(Deserialize, Serialize, Debug)]
struct TwitchAuth {
    client_id: String,
    access_token: String,
    expires_at: DateTime<Utc>,
}

struct TwitchClient {
    http_client: Client,
    client_id: String,
    client_secret: String,
    auth: TwitchAuth,
}

impl TwitchClient {
    fn new() -> Result<Self> {
        let client_id = env::var("TWITCH_CLIENT_ID").context("TWITCH_CLIENT_ID")?;
        let client_secret = env::var("TWITCH_CLIENT_SECRET").context("TWITCH_CLIENT_SECRET")?;
        let http_client = Client::new();
        let auth = Self::get_token(&http_client, &client_id, &client_secret)?;
        Ok(Self {
            http_client,
            client_id,
            client_secret,
            auth,
        })
    }

    fn ensure_token(self: &mut Self) -> Result<()> {
        if self.auth.expires_at > Utc::now() - Duration::seconds(5) {
            self.auth =
                Self::get_fresh_token(&self.http_client, &self.client_id, &self.client_secret)?;
            Self::cache_token(&self.auth)?;
        }
        Ok(())
    }

    /// Get an existing token from the cached file, or fetch a
    /// new one from the twitch API if expired or not present.
    /// When getting a new file, will also cache it locally.
    fn get_token(
        http_client: &Client,
        client_id: &String,
        client_secret: &String,
    ) -> Result<TwitchAuth> {
        Self::get_cached_token().and_then(|mb_auth| match mb_auth {
            Some(auth) => Ok(auth),
            None => {
                let auth = Self::get_fresh_token(http_client, client_id, client_secret)?;
                Self::cache_token(&auth)?;
                Ok(auth)
            }
        })
    }

    fn get_fresh_token(
        client: &Client,
        client_id: &str,
        client_secret: &str,
    ) -> Result<TwitchAuth> {
        let req = client
            .post("https://id.twitch.tv/oauth2/token")
            .query(&[("client_id", &client_id), ("client_secret", &client_secret)])
            .query(&[("grant_type", "client_credentials")]);

        let tok: TokenResponse = req.send()?.json()?;
        let auth = TwitchAuth {
            client_id: client_id.to_string(),
            access_token: tok.access_token,
            expires_at: Utc::now() + Duration::seconds(tok.expires_in as _),
        };
        Ok(auth)
    }

    fn get_cached_token() -> Result<Option<TwitchAuth>> {
        let cache_auth_path = Self::get_cache_token_path()?;
        let result = match std::fs::read_to_string(&cache_auth_path) {
            Ok(content) => {
                log::debug!("found a cached token");
                match serde_json::from_str(&content) {
                    Ok(auth) => {
                        let auth: TwitchAuth = auth; // somehow rustc wants a type annotation -_-
                        if Utc::now() >= auth.expires_at {
                            log::info!("Token expired, getting a new one");
                            None
                        } else {
                            Some(auth)
                        }
                    }
                    Err(_err) => {
                        log::info!("Cannot parse token from cache, getting a fresh one");
                        None
                    }
                }
            }
            Err(_) => {
                log::info!("Cannot open {:?}, get a fresh token", cache_auth_path);
                None
            }
        };
        Ok(result)
    }

    fn get_cache_token_path() -> Result<path::PathBuf> {
        let project_dirs =
            directories::ProjectDirs::from("geekingfrog", "geekingfrog", "twitch-notif-daemon")
                .ok_or(anyhow!("cannot construct project directories"))?;

        let cache_file = project_dirs.cache_dir().join("cached_token.json");
        Ok(cache_file)
    }

    fn cache_token(auth: &TwitchAuth) -> Result<()> {
        let cache_auth_path = Self::get_cache_token_path()?;
        match cache_auth_path.parent() {
            Some(parent) => std::fs::create_dir_all(parent)?,
            None => (),
        };
        std::fs::write(&cache_auth_path, serde_json::to_vec(&auth)?)
            .context(format!("Cannot write token to {:?}", &cache_auth_path))?;
        Ok(())
    }

    fn get_streams_data<T: Serialize + Copy>(
        self: &mut Self,
        user_logins: &[T],
    ) -> Result<TwitchResponseData<StreamResponseData>> {
        self.ensure_token()?;
        let query: Vec<_> = user_logins.iter().map(|u| ("user_login", *u)).collect();
        let req = self
            .http_client
            .get("https://api.twitch.tv/helix/streams")
            .header("Client-Id", &self.auth.client_id)
            .bearer_auth(&self.auth.access_token)
            .query(&query);

        Ok(req.send()?.json()?)
    }

    fn get_users<T: Serialize + Copy>(
        self: &mut Self,
        user_logins: &[T]
    ) -> Result<TwitchResponseData<UserResponseData>> {
        self.ensure_token()?;
        let query: Vec<_> = user_logins.iter().map(|u| ("login", *u)).collect();
        let req = self
            .http_client
            .get("https://api.twitch.tv/helix/users")
            .header("Client-Id", &self.auth.client_id)
            .bearer_auth(&self.auth.access_token)
            .query(&query);

        Ok(req.send()?.json()?)
    }
}

#[derive(StructOpt, Debug)]
#[structopt(name = "twitch streams watcher")]
struct Opt {
    #[structopt(help = "space separated list of streams to watch")]
    target_streams: Vec<String>,
}

fn main() -> Result<()> {
    env_logger::init();
    let opt = Opt::from_args();

    let mut twitch_client = TwitchClient::new()?;
    // let target_stream = "gikiam";
    let appname = "stream watcher";
    let d = Duration::seconds(10).to_std()?;

    let target_streams = opt.target_streams.iter().map(|x| &**x).collect::<Vec<_>>();
    let users = twitch_client.get_users(&target_streams[..])?.data;
    let user_map = users.iter().map(|u| (u.id.clone(), u)).collect::<BTreeMap<_,_>>();
    log::debug!("user ids: {:#?}", users);

    let stream_resp = twitch_client.get_streams_data(&target_streams[..])?;

    let mut viewer_counts: BTreeMap<_, _> = stream_resp
        .data
        .iter()
        .map(|d| (d.user_id.clone(), d.viewer_count))
        .collect();

    log::debug!("viewer counts: {:#?}", viewer_counts);
    let init_msg = users.iter().map(|user| {
        let count = viewer_counts.get(&user.id).unwrap_or(&0);
        if *count == 0 {
            format!("{} (no viewer)", user.display_name)
        } else if *count == 1 {
            format!("{} (1 viewer)", user.display_name)
        } else {
            format!("{} ({} viewer)", user.display_name, count)
        }
    }).collect::<Vec<_>>().join("\n");

    log::debug!("init message: {}", init_msg);

    Notification::new()
        .summary(appname)
        .body(&format!(
            "Start monitoring some streams !\n{}",
            init_msg
        ))
        .appname(appname)
        .show()?;


    // alternate endpoint to get the list of registered users on the chat
    // https://tmi.twitch.tv/group/user/USERNAME/chatters

    loop {
        let data = twitch_client.get_streams_data(&target_streams[..])?.data;
        let current_viewer_counts: BTreeMap<_, _> = data
            .iter()
            .map(|d| (d.user_id.clone(), d.viewer_count))
            .collect();

        for (user_id, user) in user_map.iter() {
            let prev_count = viewer_counts.get(user_id).unwrap_or(&0);
            let current_count = current_viewer_counts.get(user_id).unwrap_or(&0);
            log::debug!("current viewer count for {}: {}", user.display_name, current_count);
            if prev_count != current_count {
                Notification::new()
                    .summary(&user.display_name)
                    .body(&format!("Updated viewer count: {}", current_count))
                    .show()?;
            }
        }

        viewer_counts = current_viewer_counts;
        std::thread::sleep(d);
    }
}
