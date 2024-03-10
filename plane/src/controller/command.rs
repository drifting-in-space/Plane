use crate::{
    names::{ControllerName, Name},
    types::ClusterName,
};
use anyhow::Result;
use clap::Parser;
use std::net::IpAddr;
use url::Url;

use super::ControllerConfig;

#[derive(Parser)]
pub struct ControllerOpts {
    #[clap(long)]
    db: String,

    #[clap(long, default_value = "8080")]
    port: u16,

    #[clap(long, default_value = "127.0.0.1")]
    host: IpAddr,

    #[clap(long)]
    controller_url: Option<Url>,

    #[clap(long)]
    default_cluster: Option<ClusterName>,

    #[clap(long)]
    cleanup_min_age_days: Option<i32>,
}

impl ControllerOpts {
    pub fn into_config(self) -> Result<ControllerConfig> {
        let name = ControllerName::new_random();

        let controller_url = match self.controller_url {
            Some(url) => url,
            None => Url::parse(&format!("http://{}:{}", self.host, self.port))?,
        };

        let addr = (self.host, self.port).into();

        Ok(ControllerConfig {
            db_url: self.db,
            bind_addr: addr,
            id: name,
            controller_url,
            default_cluster: self.default_cluster,
            cleanup_min_age_days: self.cleanup_min_age_days,
        })
    }
}
