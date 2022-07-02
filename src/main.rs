use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;
use std::{env, fs};

use clap::Parser;
use eyre::{eyre, WrapErr};
use futures::future;
use kuchiki::traits::TendrilSink;
use log::{error, info};
use reqwest::Client;
use rss::{Channel, ChannelBuilder, ItemBuilder};
use serde::Deserialize;
use simple_eyre::eyre;

#[derive(Debug, Deserialize)]
struct Config {
    feed: Vec<ChannelConfig>,
}

#[derive(Debug, Deserialize)]
struct ChannelConfig {
    title: String,
    config: FeedConfig,
}

// TODO: Rename?
#[derive(Debug, Deserialize)]
struct FeedConfig {
    url: String,
    item: String,
    heading: String,
    summary: Option<String>,
    date: Option<String>,
}

/// Generate an RSS feed from websites
#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
struct Cli {
    /// path to configuration file
    #[clap(short, long, value_parser)]
    config: Option<PathBuf>,
}

const RSSPLS_LOG: &str = "RSSPLS_LOG";

#[tokio::main]
async fn main() -> ExitCode {
    match try_main().await {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(report) => {
            error!("{:?}", report);
            ExitCode::FAILURE
        }
    }
}

async fn try_main() -> eyre::Result<bool> {
    simple_eyre::install()?;
    match env::var_os(RSSPLS_LOG) {
        None => env::set_var(RSSPLS_LOG, "info"),
        Some(_) => {}
    }
    pretty_env_logger::try_init_custom_env(RSSPLS_LOG)?;

    let cli = Cli::parse();

    let config_path = cli
        .config
        .ok_or_else(|| eyre!("--config is required (for now)"))?;
    let raw_config = fs::read(&config_path).wrap_err_with(|| {
        format!(
            "unable to read configuration file: {}",
            config_path.display()
        )
    })?;
    let config: Config = toml::from_slice(&raw_config).wrap_err_with(|| {
        format!(
            "unable to parse configuration file: {}",
            config_path.display()
        )
    })?;

    let connect_timeout = Duration::from_secs(10);
    let timeout = Duration::from_secs(30);
    let client = Client::builder()
        .connect_timeout(connect_timeout)
        .timeout(timeout)
        .build()
        .wrap_err("unable to build HTTP client")?;

    let futures = config.feed.into_iter().map(|feed| {
        let client = client.clone(); // Client uses Arc internally
        tokio::spawn(async move {
            let res = process(&client, &feed).await;
            let res = res.and_then(|ref channel| {
                // TODO: channel.validate()
                let mut stdout = std::io::stdout().lock();
                channel
                    .write_to(&mut stdout)
                    .map(drop)
                    .wrap_err_with(|| format!("unable to write feed for {}", feed.config.url))
            });

            if let Err(ref report) = res {
                error!("{:?}", report);
            }
            res.is_ok()
        })
    });

    // Run all the futures at the same time
    // The ? here will fail on an error if the JoinHandle fails
    let ok = future::try_join_all(futures)
        .await?
        .into_iter()
        .fold(true, |ok, succeeded| ok & succeeded);

    Ok(ok)
}

async fn process(client: &Client, channel_config: &ChannelConfig) -> eyre::Result<Channel> {
    let config = &channel_config.config;
    info!("processing {}", config.url);
    let resp = client
        .get(&config.url)
        .send()
        .await
        .wrap_err_with(|| format!("unable to fetch {}", config.url))?;

    // Check response
    let status = resp.status();
    if !status.is_success() {
        return Err(eyre!(
            "failed to fetch {}: {} {}",
            config.url,
            status.as_str(),
            status.canonical_reason().unwrap_or("Unknown Status")
        ));
    }

    // Read body
    let html = resp.text().await.wrap_err("unable to read response body")?;

    let doc = kuchiki::parse_html().one(html);
    let mut items = Vec::new();
    for item in doc
        .select(&config.item)
        .map_err(|()| eyre!("invalid selector for item: {}", config.item))?
    {
        let title = item
            .as_node()
            .select_first(&config.heading)
            .map_err(|()| eyre!("invalid selector for title: {}", config.heading))?;

        // TODO: Need to make links absolute (probably ones in content too)
        let attrs = title.attributes.borrow();
        let link = attrs
            .get("href")
            .ok_or_else(|| eyre!("element selected as heading has no 'href' attribute"))?;

        let summary = config
            .summary
            .as_ref()
            .map(|selector| {
                item.as_node()
                    .select_first(selector)
                    .map_err(|()| eyre!("invalid selector for summary: {}", selector))
            })
            .transpose()?;
        let date = config
            .date
            .as_ref()
            .map(|selector| {
                item.as_node()
                    .select_first(selector)
                    .map_err(|()| eyre!("invalid selector for date: {}", selector))
            })
            .transpose()?;

        let rss_item = ItemBuilder::default()
            .title(title.text_contents())
            .link(Some(link.to_string()))
            .pub_date(date.map(|node| node.text_contents())) // TODO: Format as RFC 2822 date
            .content(summary.map(|node| node.text_contents()))
            .build();
        items.push(rss_item);
    }

    let channel = ChannelBuilder::default()
        .title(&channel_config.title)
        .generator(Some(String::from("RSS Please")))
        .items(items)
        .build();

    Ok(channel)
}
