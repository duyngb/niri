use crate::niri::State;
use anyhow::bail;
use zbus::{fdo, proxy};

#[proxy(
    default_path = "/org/mpris/MediaPlayer2",
    interface = "org.mpris.MediaPlayer2.Player"
)]
pub trait MprisPlayer {
    fn play_pause(&self) -> zbus::Result<()>;
    fn pause(&self) -> zbus::Result<()>;
    fn play(&self) -> zbus::Result<()>;
    fn stop(&self) -> zbus::Result<()>;

    fn next(&self) -> zbus::Result<()>;
    fn previous(&self) -> zbus::Result<()>;

    #[zbus(property)]
    fn playback_status(&self) -> fdo::Result<String>;
}

pub fn new_proxy<'a>(state: &State, player: &str) -> anyhow::Result<MprisPlayerProxyBlocking<'a>> {
    let Some(dbus) = state.niri.dbus.as_ref() else {
        bail!("bus unavailable");
    };

    let Some(conn) = dbus.client_conn.as_ref() else {
        bail!("client connection unavailable");
    };

    let full_name = format!("org.mpris.MediaPlayer2.{}", player);
    let proxy = MprisPlayerProxyBlocking::builder(conn)
        .destination(full_name)?
        .build()?;
    Ok(proxy)
}

pub fn play_pause<'a>(state: &State, player: &str) {
    call_method(state, player, MprisPlayerProxyBlocking::play_pause);
}

pub fn pause<'a>(state: &State, player: &str) {
    call_method(state, player, MprisPlayerProxyBlocking::pause);
}

pub fn play<'a>(state: &State, player: &str) {
    call_method(state, player, MprisPlayerProxyBlocking::play);
}

pub fn stop<'a>(state: &State, player: &str) {
    call_method(state, player, MprisPlayerProxyBlocking::stop);
}

pub fn next<'a>(state: &State, player: &str) {
    call_method(state, player, MprisPlayerProxyBlocking::next);
}

pub fn previous<'a>(state: &State, player: &str) {
    call_method(state, player, MprisPlayerProxyBlocking::previous);
}

fn call_method<'a, F>(state: &State, player: &str, method: F)
where
    F: FnOnce(&MprisPlayerProxyBlocking<'a>) -> zbus::Result<()>,
{
    // FIXME: can we cache the proxy object?
    let Ok(proxy) = new_proxy(state, player) else {
        // it's probably ok to not getting a named proxy here
        return;
    };

    if let Err(e) = method(&proxy) {
        debug!("failed to call mpris method on player {player}: {e:?}");
    }
}
