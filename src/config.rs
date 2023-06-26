use std::{convert::TryFrom, fmt::Display, fs::OpenOptions, io::Read as _, str::FromStr};

use ckb_types::{packed, prelude::*, H256};

use crate::{
    error::{Error, Result},
    types::RunEnv,
};

pub(crate) enum AppConfig {
    Run(RunConfig),
}

pub(crate) struct RunConfig {
    pub(crate) run_env: RunEnv,
    pub(crate) trusted_hash_opt: Option<packed::Byte32>,
}

impl AppConfig {
    pub(crate) fn load() -> Result<Self> {
        let yaml = clap::load_yaml!("cli.yaml");
        let matches = clap::App::from_yaml(yaml)
            .version(clap::crate_version!())
            .author(clap::crate_authors!("\n"))
            .about(clap::crate_description!())
            .get_matches();
        Self::try_from(&matches)
    }

    pub(crate) fn execute(self) -> Result<()> {
        log::info!("Executing ...");
        match self {
            Self::Run(cfg) => cfg.execute(),
        }
    }
}

impl<'a> TryFrom<&'a clap::ArgMatches<'a>> for AppConfig {
    type Error = Error;
    fn try_from(matches: &'a clap::ArgMatches) -> Result<Self> {
        match matches.subcommand() {
            ("run", Some(submatches)) => RunConfig::try_from(submatches).map(AppConfig::Run),
            (subcmd, _) => Err(Error::config(format!("subcommand {}", subcmd))),
        }
    }
}

impl<'a> TryFrom<&'a clap::ArgMatches<'a>> for RunConfig {
    type Error = Error;
    fn try_from(matches: &'a clap::ArgMatches) -> Result<Self> {
        let run_env = parse_from_file::<RunEnv>(matches, "config-file")?;
        let trusted_hash_opt = parse_option::<H256>(matches, "trusted-hash")?.map(|h| h.pack());
        Ok(Self {
            run_env,
            trusted_hash_opt,
        })
    }
}

fn parse_option<T: FromStr>(matches: &clap::ArgMatches, name: &str) -> Result<Option<T>>
where
    <T as FromStr>::Err: Display,
{
    matches
        .value_of(name)
        .map(|data| T::from_str(&data).map_err(Error::config))
        .transpose()
}

fn parse_from_file<T: FromStr>(matches: &clap::ArgMatches, name: &str) -> Result<T>
where
    <T as FromStr>::Err: Display,
{
    matches
        .value_of(name)
        .map(|file| {
            OpenOptions::new()
                .read(true)
                .open(file)
                .map_err(|err| Error::config(format!("failed to open {} since {}", file, err)))
                .and_then(|mut f| {
                    let mut buffer = String::new();
                    f.read_to_string(&mut buffer)
                        .map_err(|err| {
                            Error::config(format!("failed to read {} since {}", file, err))
                        })
                        .map(|_| buffer)
                })
                .and_then(|data| T::from_str(&data).map_err(Error::config))
        })
        .transpose()?
        .ok_or_else(|| Error::argument_should_exist(name))
}
