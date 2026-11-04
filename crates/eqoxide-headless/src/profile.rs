//! Private launch configuration. Parse errors never echo the document or credentials.
use eqoxide_core::config::{CharacterCreate, LoginConfig};
use eqoxide_net::login::ExpectedWorld;
use serde::Deserialize;
use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub login: Login,
    pub expected_world: World,
    pub api_port: u16,
    pub control_token: String,
    pub maps_dir: PathBuf,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct World {
    pub name: String,
    pub host: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Login {
    pub login_host: String,
    pub login_port: u16,
    pub world_port: u16,
    pub username: String,
    pub password: String,
    pub character_name: String,
    pub character_create: Option<Create>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Create {
    pub race: u32,
    pub class: u32,
    pub gender: u32,
    pub deity: u32,
    pub start_zone: u32,
    pub str: u32,
    pub sta: u32,
    pub agi: u32,
    pub dex: u32,
    pub wis: u32,
    pub int: u32,
    pub cha: u32,
    #[serde(default)]
    pub face: u32,
    #[serde(default)]
    pub hairstyle: u32,
    #[serde(default)]
    pub haircolor: u32,
    #[serde(default)]
    pub beard: u32,
    #[serde(default)]
    pub beardcolor: u32,
    #[serde(default)]
    pub eyecolor1: u32,
    #[serde(default)]
    pub eyecolor2: u32,
}
impl Profile {
    pub fn load(path: &Path) -> Result<Self, String> {
        let meta = std::fs::symlink_metadata(path).map_err(|_| "cannot stat private profile")?;
        if !meta.is_file() || meta.permissions().mode() & 0o077 != 0 || meta.len() > 64 * 1024 {
            return Err("profile must be a private regular file (mode 600, at most 64KiB)".into());
        }
        let bytes = std::fs::read(path).map_err(|_| "cannot read private profile")?;
        let profile: Self =
            serde_json::from_slice(&bytes).map_err(|_| "invalid launch profile JSON/schema")?;
        profile.validate()?;
        Ok(profile)
    }
    fn validate(&self) -> Result<(), String> {
        if self.api_port == 0
            || self.control_token.len() < 32
            || self.control_token.len() > 256
            || !self.control_token.bytes().all(|c| c.is_ascii_graphic())
            || self.expected_world.name.is_empty()
            || self.expected_world.host.is_empty()
            || self.login.login_port == 0
            || self.login.world_port == 0
            || self.login.username.is_empty()
            || self.login.password.is_empty()
            || !self.maps_dir.is_dir()
        {
            return Err("invalid launch profile settings".into());
        }
        let cfg = self.login_config();
        cfg.check_character_name()
            .map_err(|_| "invalid character name for normal creation")?;
        Ok(())
    }
    pub fn expected(&self) -> ExpectedWorld {
        ExpectedWorld {
            name: self.expected_world.name.clone(),
            host: self.expected_world.host.clone(),
        }
    }
    pub fn login_config(&self) -> LoginConfig {
        LoginConfig {
            login_host: self.login.login_host.clone(),
            login_port: self.login.login_port,
            world_port: self.login.world_port,
            username: self.login.username.clone(),
            password: self.login.password.clone(),
            character_name: self.login.character_name.clone(),
            create: self
                .login
                .character_create
                .as_ref()
                .map(|c| CharacterCreate {
                    race: c.race,
                    class: c.class,
                    gender: c.gender,
                    deity: c.deity,
                    start_zone: c.start_zone,
                    str_: c.str,
                    sta: c.sta,
                    agi: c.agi,
                    dex: c.dex,
                    wis: c.wis,
                    int_: c.int,
                    cha: c.cha,
                    face: c.face,
                    hairstyle: c.hairstyle,
                    haircolor: c.haircolor,
                    beard: c.beard,
                    beardcolor: c.beardcolor,
                    eyecolor1: c.eyecolor1,
                    eyecolor2: c.eyecolor2,
                }),
        }
    }
}
