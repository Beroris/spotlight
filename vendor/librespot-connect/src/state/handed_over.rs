//! Spotlight: keeping the songs coming up when the music moves here from another device.
//!
//! A playlist or album has an address Spotify can be asked for, and librespot does that. Songs
//! started as a plain list have none: they play under "spotify:web-api" (or "-" from Spotify's
//! own apps), which can't be looked up. That is how Spotlight starts most of its music, and
//! librespot then kept only the song playing, so the rest of the list was gone after every
//! switch of device. The songs are still handed over, in two places, and this reads both.

use crate::{
    core::{Error, SpotifyId, SpotifyUri},
    protocol::{
        context_track::ContextTrack,
        player::{PlayerState, ProvidedTrack},
        transfer_state::TransferState,
    },
    state::{
        ConnectState, SPOTIFY_MAX_NEXT_TRACKS_SIZE, SPOTIFY_MAX_PREV_TRACKS_SIZE,
        context::ContextType,
        metadata::Metadata,
        provider::{IsProvider, Provider},
        tracks::IDENTIFIER_DELIMITER,
    },
};
use std::collections::HashSet;

/// The list another device was playing, as far as Spotify passes it on.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct HandedOver {
    /// The list's songs, in the order they play.
    pub tracks: Vec<ContextTrack>,
    /// Where the song playing is in `tracks`. `None` while a queued song plays.
    pub current: Option<usize>,
    /// Where in `tracks` the list carries on once the song playing ends.
    pub resume_at: usize,
    /// The queued songs coming up, soonest first.
    pub queued: Vec<Queued>,
    /// Where it was read, for the log.
    pub read_from: &'static str,
}

/// A queued song coming up. It can have been moved down among the list's songs.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Queued {
    /// How many of the list's songs play before it.
    pub after: usize,
    pub track: ContextTrack,
}

const THE_REPORT: &str = "what the other device reported";
const THE_HANDOVER: &str = "the handover";
const BOTH: &str = "the handover, with the songs played from what the other device reported";

impl HandedOver {
    /// The list to carry on with: what Spotify last reported of the device that had the music
    /// (`elsewhere`), or what the handover itself brings. The report also has the songs
    /// already played, so the previous button works, and more of the songs coming up. It is
    /// used when it is of the same song (`current_uri`) and the handover, the newer of the
    /// two, has the same songs coming up as far as it goes. When they differ the queue was
    /// changed just before the switch: the songs coming up are then the handover's, and only
    /// the songs played are the report's.
    pub fn of(
        transfer: &TransferState,
        elsewhere: Option<&PlayerState>,
        current_uri: &str,
    ) -> Option<Self> {
        let seen = elsewhere
            .filter(|player| !current_uri.is_empty() && player.track.uri == current_uri)
            .and_then(Self::from_player);
        let told = Self::from_transfer(transfer);
        match (seen, told) {
            (Some(seen), Some(told)) => {
                // A handover with only the song in it says nothing about the rest.
                let agree = told.coming_up().is_empty() || seen.comes_up_as(&told);
                Some(if agree { seen } else { told.after_the_played_of(seen) })
            }
            (seen, told) => seen.or(told),
        }
    }

    /// Whether `uri` is a playlist, album or other list Spotify can be asked for.
    pub fn can_be_asked_for(uri: &str) -> bool {
        const MADE_UP: [&str; 4] = ["", "-", "spotify:web-api", "spotify:unknown"];
        // Spotify's own API names a plain list of songs "spotify:webapi", or that with an id
        // after it ("spotify:webapi:3W3w..."). Asking for either is answered with "400 Bad
        // Request", seen in the app's log at every start of such a list.
        let from_the_api = uri == "spotify:webapi" || uri.starts_with("spotify:webapi:");
        !MADE_UP.contains(&uri) && !from_the_api && !uri.starts_with("spotify:search")
    }

    /// The songs coming up in the order they play: the list's, with the queued ones where
    /// they are among them.
    fn coming_up(&self) -> Vec<&ContextTrack> {
        let listed = self.tracks.get(self.resume_at..).unwrap_or_default();
        let mut queued = self.queued.iter().peekable();
        let mut coming = Vec::with_capacity(listed.len() + self.queued.len());
        for (before, track) in listed.iter().enumerate() {
            while let Some(next) = queued.next_if(|queued| queued.after <= before) {
                coming.push(&next.track);
            }
            coming.push(track);
        }
        coming.extend(queued.map(|queued| &queued.track));
        coming
    }

    /// Whether the songs coming up are those of `newer`, as far as it goes.
    fn comes_up_as(&self, newer: &Self) -> bool {
        let (mine, theirs) = (self.coming_up(), newer.coming_up());
        mine.len() >= theirs.len() && mine.iter().zip(&theirs).all(|(mine, theirs)| same_entry(mine, theirs))
    }

    /// This list, a handover's, with the songs `report` has before the song playing put in
    /// front of it, as a handover begins at the song playing. A song played that the handover
    /// has coming up is left out of the songs played: the two sources can show one entry at
    /// two moments, and nothing tells that from a song that is in the list twice.
    fn after_the_played_of(mut self, report: Self) -> Self {
        let (Some(at), Some(reported_at)) = (self.current, report.current) else {
            return self;
        };
        let mut tracks = report.tracks;
        tracks.truncate(reported_at);
        let mine = self.tracks.split_off(at.min(self.tracks.len()));
        let known = Known::of(&mine);
        tracks.retain(|track| !known.has(track));
        let played = tracks.len();
        tracks.extend(mine);
        Self {
            tracks,
            current: Some(played),
            resume_at: played + 1,
            queued: self.queued,
            read_from: BOTH,
        }
    }

    /// With repeat on a list follows itself, which shows how it starts: the songs from before
    /// the few a device keeps as played. `again` is the list as it comes round, and what
    /// isn't known of it yet goes in front.
    fn begin_with(&mut self, mut again: Vec<ContextTrack>) {
        // The songs known so far are the list from some song on, so they are where `again`
        // carries on with the same songs in the same order. A song can be in a list twice,
        // and it is its place in that run that tells the two apart.
        let known_from = (0..again.len()).find(|&from| {
            again[from..]
                .iter()
                .zip(&self.tracks)
                .all(|(again, known)| same_entry(again, known))
        });
        let start = match known_from {
            Some(from) => {
                again.truncate(from);
                again
            }
            // The order was changed since. What is left to go by is which songs are known.
            None => {
                let known = Known::of(&self.tracks);
                again.into_iter().filter(|track| !known.has(track)).collect()
            }
        };
        self.current = self.current.map(|at| at + start.len());
        self.resume_at += start.len();
        self.tracks.splice(0..0, start);
    }

    /// From a device's own report of what it plays. Its songs coming up run on past the end
    /// of the list when repeat is on, with a hidden marker where the list begins again.
    pub fn from_player(player: &PlayerState) -> Option<Self> {
        // The songs played since the list last began.
        let played = player
            .prev_tracks
            .rsplit(is_marker)
            .next()
            .unwrap_or_default();
        let mut tracks: Vec<ContextTrack> = played.iter().filter_map(listed).collect();

        let playing = player.track.as_ref().and_then(listed);
        let current = playing.is_some().then_some(tracks.len());
        tracks.extend(playing);
        let resume_at = tracks.len();

        let mut queued = Vec::new();
        // The songs coming up that aren't queued, counted on past where the list begins
        // again: a queued song can have been moved down that far.
        let mut before = 0;
        let mut begun_again = false;
        for track in &player.next_tracks {
            if is_marker(track) {
                begun_again = true;
                continue;
            }
            let Some(song) = song(track) else { continue };
            if track.is_queue() {
                queued.push(Queued { after: before, track: song });
                continue;
            }
            before += 1;
            if !begun_again {
                tracks.push(song);
            }
        }
        let again = player.next_tracks.split(is_marker).nth(1).unwrap_or_default();

        // A list is needed to carry on from. Queued songs alone come over with the handover.
        if tracks.is_empty() {
            return None;
        }
        let mut list = Self {
            tracks,
            current,
            resume_at,
            queued,
            read_from: THE_REPORT,
        };
        // Similar songs added after a list that has ended come after a marker too, and
        // aren't part of it.
        let similar_songs = player.track.is_autoplay();
        if !similar_songs && again.iter().all(|track| track.is_context() || track.is_queue()) {
            list.begin_with(again.iter().filter_map(listed).collect());
        }
        Some(list)
    }

    /// From the handover: Spotify puts the song playing and those after it into the pages of
    /// the list it names, and queued songs into the queue.
    pub fn from_transfer(transfer: &TransferState) -> Option<Self> {
        let session = transfer.current_session.as_ref()?;
        let playing = transfer.playback.current_track.as_ref().and_then(plain);
        let playing_queued = transfer.queue.is_playing_queue.unwrap_or_default();

        let mut tracks = Vec::new();
        // For now each says how many of the pages' songs are before it.
        let mut queued_in_pages = Vec::new();
        // The list as it comes round again with repeat on, and whether what follows the
        // marker is similar songs instead.
        let mut again = Vec::new();
        let mut similar_songs = false;
        let mut round = 0;
        // The pages' songs so far that aren't queued, counted on past where the list begins
        // again.
        let mut before = 0;
        for track in session.context.pages.iter().flat_map(|page| &page.tracks) {
            if is_page_marker(track) {
                round += 1;
                if round > 1 {
                    break;
                }
                continue;
            }
            let Some(plain) = plain(track) else { continue };
            let provider = track.metadata.get("provider").map(String::as_str);
            if provider == Some("queue") {
                queued_in_pages.push(Queued { after: before, track: plain });
                continue;
            }
            before += 1;
            match provider {
                _ if round == 0 => tracks.push(plain),
                Some("autoplay") => similar_songs = true,
                _ => again.push(plain),
            }
        }

        let current_uid = session.current_uid.as_deref().filter(|uid| !uid.is_empty());
        let marked = current_uid
            .and_then(|uid| tracks.iter().position(|track| track.uid.as_deref() == Some(uid)));
        // How many of the pages' songs are over with: all up to the song playing.
        let mut over = 0;
        let (current, resume_at) = if playing_queued {
            // While a queued song plays, the mark is on the song the list carries on with.
            over = marked.unwrap_or(0);
            (None, over)
        } else {
            // By its mark, or failing that by which song it is.
            let at = marked.or_else(|| {
                let playing = playing.as_ref()?;
                tracks.iter().position(|track| track.uri == playing.uri)
            });
            match (at, playing.clone()) {
                (Some(at), _) => {
                    over = at + 1;
                    (Some(at), over)
                }
                // The pages start after the song playing.
                (None, Some(playing)) => {
                    tracks.insert(0, playing);
                    (Some(0), 1)
                }
                (None, None) => (None, 0),
            }
        };
        // What counts for a queued song is how many of the songs coming up are before it. One
        // from before the place the list carries on from has played.
        queued_in_pages.retain(|queued| queued.after >= over);
        for queued in &mut queued_in_pages {
            queued.after -= over;
        }
        let coming_up_in_pages = before.saturating_sub(over);

        let mut queued: Vec<Queued> = if transfer.queue.tracks.is_empty() {
            let playing_uid = playing.as_ref().and_then(|playing| playing.uid.as_deref());
            queued_in_pages
                .into_iter()
                .filter(|queued| !playing_queued || queued.track.uid.as_deref() != playing_uid)
                .collect()
        } else {
            // The queue says which songs are queued, and the pages where each is among the
            // others. With no queued songs in the pages they are all next. With some, one
            // that isn't there comes after the songs the pages go up to: no later, or it
            // would fall off the end of a long list.
            let unplaced = if queued_in_pages.is_empty() { 0 } else { coming_up_in_pages };
            // The first is the song playing, when a queued song plays.
            let skip = usize::from(playing_queued);
            transfer
                .queue
                .tracks
                .iter()
                .skip(skip)
                .filter_map(plain)
                .map(|track| Queued {
                    after: queued_in_pages
                        .iter()
                        .find(|placed| same_entry(&placed.track, &track))
                        .map_or(unplaced, |placed| placed.after),
                    track,
                })
                .collect()
        };
        // Soonest first, as the pages have them.
        queued.sort_by_key(|queued| queued.after);

        if tracks.is_empty() {
            return None;
        }
        let mut list = Self {
            tracks,
            current,
            resume_at,
            queued,
            read_from: THE_HANDOVER,
        };
        if !similar_songs {
            list.begin_with(again);
        }
        Some(list)
    }
}

/// The songs already in a list, each known by its mark and by which song it is. A device may
/// mark a song anew each time its list comes round, and the song is then still the same.
struct Known<'a>(HashSet<&'a [u8]>);

impl<'a> Known<'a> {
    fn of(tracks: &'a [ContextTrack]) -> Self {
        Self(tracks.iter().flat_map(names).collect())
    }

    fn has(&self, track: &ContextTrack) -> bool {
        names(track).any(|name| self.0.contains(name))
    }
}

/// What a song in a list can be known by: its mark and its address.
fn names(track: &ContextTrack) -> impl Iterator<Item = &[u8]> {
    fn text(field: &Option<String>) -> Option<&[u8]> {
        field.as_deref().map(str::as_bytes)
    }
    [text(&track.uid), text(&track.uri)]
        .into_iter()
        .flatten()
        .filter(|name| !name.is_empty())
}

/// Whether two entries are the same song by any of those.
fn same_entry(a: &ContextTrack, b: &ContextTrack) -> bool {
    names(a).any(|name| names(b).any(|other| other == name))
}

/// The hidden entry a device puts where its list begins again.
fn is_marker(track: &ProvidedTrack) -> bool {
    track.uid.starts_with(IDENTIFIER_DELIMITER) || track.uri.ends_with(IDENTIFIER_DELIMITER)
}

fn is_page_marker(track: &ContextTrack) -> bool {
    track.uid.as_deref().is_some_and(|uid| uid.starts_with(IDENTIFIER_DELIMITER))
        || track.uri.as_deref().is_some_and(|uri| uri.ends_with(IDENTIFIER_DELIMITER))
}

/// A song as it goes into a list: only what says which song it is. What else a device kept
/// with it (where it came from, whether it was queued) is that device's business.
fn song(track: &ProvidedTrack) -> Option<ContextTrack> {
    let playable = !track.is_unavailable()
        && !is_marker(track)
        && !track.uri.contains('?')
        && SpotifyUri::from_uri(&track.uri).is_ok();
    playable.then(|| ContextTrack {
        uri: Some(track.uri.clone()),
        uid: Some(track.uid.clone()),
        ..Default::default()
    })
}

/// A song that belongs to the list, which a queued one doesn't.
fn listed(track: &ProvidedTrack) -> Option<ContextTrack> {
    song(track).filter(|_| !track.is_queue())
}

/// The same for a song from a handover. It can come with only Spotify's own id for it, and
/// gets its address from that, so that every song in a list is known the same way.
fn plain(track: &ContextTrack) -> Option<ContextTrack> {
    let uri = track.uri.as_deref().filter(|uri| !uri.is_empty());
    let gid = track.gid.as_deref().filter(|gid| !gid.is_empty());
    let uri = match (uri, gid) {
        (Some(uri), _) if !uri.contains('?') && SpotifyUri::from_uri(uri).is_ok() => uri.to_string(),
        (None, Some(gid)) => {
            let id = SpotifyId::from_raw(gid).ok()?;
            SpotifyUri::Track { id }.to_uri().ok()?
        }
        _ => return None,
    };
    Some(ContextTrack {
        uri: Some(uri),
        uid: track.uid.clone(),
        ..Default::default()
    })
}

impl ConnectState {
    /// Carries on from a list that was handed over and has just been loaded: the song playing,
    /// the songs before it for the previous button, then the rest of the list with the queued
    /// songs where they were in it. A shuffled list keeps the order it was dealt.
    pub fn carry_on_from(&mut self, list: &HandedOver) -> Result<(), Error> {
        self.set_active_context(ContextType::Default);
        self.fill_up_context = ContextType::Default;

        match list.current {
            Some(at) => self.set_current_track(at)?,
            // A queued song plays, and stays as the handover gave it.
            None => self.player_mut().index.clear(),
        }

        let context = self.get_context(ContextType::Default)?;
        let played = list.current.unwrap_or(list.resume_at).min(context.tracks.len());
        let prev_tracks = context
            .tracks
            .iter()
            .take(played)
            .skip(played.saturating_sub(SPOTIFY_MAX_PREV_TRACKS_SIZE))
            .cloned()
            .collect();
        self.set_prev_tracks(prev_tracks);
        self.update_context_index(ContextType::Default, list.resume_at)?;

        self.player_mut().next_tracks.clear();
        self.fill_up_next_tracks()?;

        let context_uri = self.context_uri().clone();
        for queued in &list.queued {
            let Ok(mut track) = self.context_to_provided_track(
                &queued.track,
                Some(&context_uri),
                None,
                None,
                Some(Provider::Queue),
            ) else {
                continue;
            };
            track.uid = format!("q{}", self.queue_count);
            self.queue_count += 1;
            track.set_from_queue(true);

            // After as many of the list's songs as it had before it, and after the queued
            // songs already put back there.
            let next_tracks = &mut self.player_mut().next_tracks;
            let mut listed_before = 0;
            let at = next_tracks
                .iter()
                .position(|next| {
                    if next.is_queue() {
                        return false;
                    }
                    let here = listed_before == queued.after;
                    listed_before += usize::from(!is_marker(next));
                    here
                })
                .unwrap_or(next_tracks.len());
            next_tracks.insert(at, track);
        }

        // Songs the queued ones pushed off the end wait in the list again.
        let next_tracks = &mut self.player_mut().next_tracks;
        let pushed_off = next_tracks.split_off(SPOTIFY_MAX_NEXT_TRACKS_SIZE.min(next_tracks.len()));
        self.return_to_context(&pushed_off);

        self.update_queue_revision();
        self.update_restrictions();
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{
        connect::Device,
        context::Context,
        context_page::ContextPage,
        player::ContextPlayerOptions,
    };
    use protobuf::MessageField;

    /// A song's address, ending in its name.
    fn uri(name: char) -> String {
        format!("spotify:track:{}{name}", "0".repeat(21))
    }

    fn provided(name: char, provider: Provider) -> ProvidedTrack {
        let mut track = ProvidedTrack {
            uri: uri(name),
            uid: format!("u{name}"),
            ..Default::default()
        };
        track.set_provider(provider);
        track
    }

    fn marker(round: u32) -> ProvidedTrack {
        ProvidedTrack {
            uri: format!("spotify:{IDENTIFIER_DELIMITER}"),
            uid: format!("{IDENTIFIER_DELIMITER}{round}"),
            provider: Provider::Context.to_string(),
            ..Default::default()
        }
    }

    /// A device's report: what it played, what plays, what comes up. Capitals are queued
    /// songs, `|` is where the list begins again and `~` starts similar songs.
    fn report(played: &str, playing: char, coming: &str) -> PlayerState {
        let mut similar = false;
        let mut tracks = |text: &str| {
            text.chars()
                .filter_map(|name| match name {
                    '|' => Some(marker(0)),
                    '~' => {
                        similar = true;
                        Some(marker(0))
                    }
                    name if name.is_uppercase() => Some(provided(name, Provider::Queue)),
                    name if similar => Some(provided(name, Provider::Autoplay)),
                    name => Some(provided(name, Provider::Context)),
                })
                .collect::<Vec<_>>()
        };
        let prev_tracks = tracks(played);
        let next_tracks = tracks(coming);
        let provider = if playing.is_uppercase() { Provider::Queue } else { Provider::Context };
        PlayerState {
            track: MessageField::some(provided(playing, provider)),
            prev_tracks,
            next_tracks,
            ..Default::default()
        }
    }

    fn letters(tracks: &[ContextTrack]) -> String {
        tracks
            .iter()
            .map(|track| track.uri.as_deref().and_then(|uri| uri.chars().last()).unwrap_or('?'))
            .collect()
    }

    fn page_track(name: char, provider: &str) -> ContextTrack {
        let mut track = ContextTrack {
            uri: Some(uri(name)),
            uid: Some(format!("u{name}")),
            ..Default::default()
        };
        track.metadata.insert("provider".to_string(), provider.to_string());
        track
    }

    /// A handover naming `context` with `pages` as its songs, on the song marked `current_uid`.
    fn handover(pages: Vec<ContextTrack>, current_uid: &str, queue: Vec<ContextTrack>) -> TransferState {
        let mut transfer = TransferState::new();
        let session = transfer.current_session.mut_or_insert_default();
        session.current_uid = Some(current_uid.to_string());
        let context = session.context.mut_or_insert_default();
        context.uri = Some("spotify:web-api".to_string());
        context.pages = vec![ContextPage::from(pages)];
        transfer.queue.mut_or_insert_default().tracks = queue;
        transfer
    }


    /// The queued songs coming up, each with how many of the list's songs are before it.
    fn queued(list: &HandedOver) -> String {
        list.queued
            .iter()
            .map(|queued| format!("{}{}", letters(std::slice::from_ref(&queued.track)), queued.after))
            .collect()
    }

    fn page_marker() -> ContextTrack {
        ContextTrack {
            uri: Some(format!("spotify:{IDENTIFIER_DELIMITER}")),
            uid: Some(format!("{IDENTIFIER_DELIMITER}0")),
            ..Default::default()
        }
    }

    /// A handover's pages: capitals are queued songs and `|` is where the list begins again.
    fn pages(songs: &str) -> Vec<ContextTrack> {
        songs
            .chars()
            .map(|name| match name {
                '|' => page_marker(),
                name if name.is_uppercase() => page_track(name, "queue"),
                name => page_track(name, "context"),
            })
            .collect()
    }

    #[test]
    fn a_list_part_way_through_keeps_both_sides_of_the_song() {
        let list = HandedOver::from_player(&report("ab", 'c', "de")).unwrap();
        assert_eq!(letters(&list.tracks), "abcde");
        assert_eq!((list.current, list.resume_at), (Some(2), 3));
        assert!(list.queued.is_empty());
    }

    #[test]
    fn queued_songs_stay_queued_and_out_of_the_list() {
        let list = HandedOver::from_player(&report("a", 'b', "XYcd")).unwrap();
        assert_eq!(letters(&list.tracks), "abcd");
        assert_eq!(queued(&list), "X0Y0");
        assert_eq!((list.current, list.resume_at), (Some(1), 2));
    }

    #[test]
    fn queued_songs_moved_down_the_list_keep_their_place() {
        let list = HandedOver::from_player(&report("a", 'b', "XcYdeZ")).unwrap();
        assert_eq!(letters(&list.tracks), "abcde");
        assert_eq!(queued(&list), "X0Y1Z3");
        // Past where a repeating list begins again, the songs before it are still counted.
        let list = HandedOver::from_player(&report("a", 'b', "c|aXbc")).unwrap();
        assert_eq!((letters(&list.tracks), queued(&list)), ("abc".to_string(), "X2".to_string()));
        let mut repeating = state();
        repeating.set_repeat_context(true);
        assert!(played(carry_on(repeating, &list).next_tracks()).starts_with("c|aXbc|abc"));
    }

    #[test]
    fn a_queued_song_playing_leaves_the_list_where_it_was() {
        let list = HandedOver::from_player(&report("ab", 'X', "Ycd")).unwrap();
        assert_eq!(letters(&list.tracks), "abcd");
        assert_eq!(queued(&list), "Y0");
        assert_eq!((list.current, list.resume_at), (None, 2));
    }

    #[test]
    fn a_repeating_list_is_whole_even_on_its_last_song() {
        // Twelve songs, the last one playing. Only ten are kept as played, so the first is
        // known just from the list coming round again.
        let list = HandedOver::from_player(&report("bcdefghijk", 'l', "|abcdefghijkl|abc")).unwrap();
        assert_eq!(letters(&list.tracks), "abcdefghijkl");
        assert_eq!((list.current, list.resume_at), (Some(11), 12));
    }

    #[test]
    fn a_repeating_list_that_came_round_starts_where_it_began_again() {
        // The songs before the marker were the end of the round before.
        let list = HandedOver::from_player(&report("yz|a", 'b', "cd|abcd")).unwrap();
        assert_eq!(letters(&list.tracks), "abcd");
        assert_eq!((list.current, list.resume_at), (Some(1), 2));
    }

    #[test]
    fn a_list_marked_anew_as_it_comes_round_isn_t_doubled() {
        let mut player = report("b", 'c', "d|abcd");
        for (place, track) in player.next_tracks.iter_mut().enumerate().skip(2) {
            track.uid = format!("again{place}");
        }
        let list = HandedOver::from_player(&player).unwrap();
        assert_eq!(letters(&list.tracks), "abcd");
        assert_eq!((list.current, list.resume_at), (Some(2), 3));
    }

    #[test]
    fn a_song_that_is_in_a_repeating_list_twice_stays_in_it_twice() {
        // Twelve songs with "a" first and second to last, the last one playing. The ten kept
        // as played have only the second "a".
        let mut player = report("bcdefghija", 'k', "|abcdefghijak|ab");
        player.prev_tracks[9].uid = "second".to_string();
        player.next_tracks[11].uid = "second".to_string();
        let list = HandedOver::from_player(&player).unwrap();
        assert_eq!(letters(&list.tracks), "abcdefghijak");
        assert_eq!((list.current, list.resume_at), (Some(11), 12));
        // And when a device marks its songs anew each round.
        for (place, track) in player.next_tracks.iter_mut().enumerate().skip(1) {
            track.uid = format!("again{place}");
        }
        assert_eq!(letters(&HandedOver::from_player(&player).unwrap().tracks), "abcdefghijak");
    }

    #[test]
    fn a_repeating_list_put_in_another_order_gets_the_songs_it_lacks() {
        // The songs coming up were moved about, so they no longer show where the list is.
        let list = HandedOver::from_player(&report("", 'c', "ba|abcd")).unwrap();
        assert_eq!(letters(&list.tracks), "dcba");
        assert_eq!((list.current, list.resume_at), (Some(1), 2));
    }

    #[test]
    fn similar_songs_after_the_list_are_not_part_of_it() {
        let list = HandedOver::from_player(&report("a", 'b', "c~xyz")).unwrap();
        assert_eq!(letters(&list.tracks), "abc");
    }

    #[test]
    fn songs_that_cant_be_played_are_left_out() {
        let mut player = report("a", 'b', "cd");
        player.next_tracks[0].set_provider(Provider::Unavailable);
        player.next_tracks.push(ProvidedTrack { uri: "nonsense".into(), ..Default::default() });
        let list = HandedOver::from_player(&player).unwrap();
        assert_eq!(letters(&list.tracks), "abd");
    }

    #[test]
    fn nothing_to_carry_on_with_is_nothing() {
        let player = PlayerState::default();
        assert_eq!(HandedOver::from_player(&player), None);
        assert_eq!(HandedOver::from_transfer(&TransferState::new()), None);
    }

    #[test]
    fn a_handover_s_pages_are_the_song_and_those_after_it() {
        let list = HandedOver::from_transfer(&handover(pages("cde"), "uc", Vec::new())).unwrap();
        assert_eq!(letters(&list.tracks), "cde");
        assert_eq!((list.current, list.resume_at), (Some(0), 1));
        assert!(list.tracks.iter().all(|track| track.metadata.is_empty()), "only the song itself is kept");
    }

    #[test]
    fn a_handover_s_queue_is_used_once() {
        let list = HandedOver::from_transfer(&handover(pages("cXd"), "uc", pages("X"))).unwrap();
        assert_eq!((letters(&list.tracks), queued(&list)), ("cd".to_string(), "X0".to_string()));
        let list = HandedOver::from_transfer(&handover(pages("cXd"), "uc", Vec::new())).unwrap();
        assert_eq!((letters(&list.tracks), queued(&list)), ("cd".to_string(), "X0".to_string()));
    }

    #[test]
    fn a_handover_s_queue_takes_its_places_from_the_pages() {
        // The queue says which songs are queued, the pages where they are among the others.
        let list = HandedOver::from_transfer(&handover(pages("cdXeY"), "uc", pages("YX"))).unwrap();
        assert_eq!((letters(&list.tracks), queued(&list)), ("cde".to_string(), "X1Y2".to_string()));
        assert_eq!(played(carry_on(state(), &list).next_tracks()), "dXeY");
        // One the pages don't go as far as comes after the songs they have.
        // No later, or it would fall off the end of a long list.
        let list = HandedOver::from_transfer(&handover(pages("cXd"), "uc", pages("XY"))).unwrap();
        assert_eq!(queued(&list), "X0Y1");
        assert_eq!(played(carry_on(state(), &list).next_tracks()), "XdY");
        // Pages with no queued songs in them: the queue is next, as it always was.
        let list = HandedOver::from_transfer(&handover(pages("cd"), "uc", pages("XY"))).unwrap();
        assert_eq!(played(carry_on(state(), &list).next_tracks()), "XYd");
        // Past where a repeating list begins again, the songs before it are still counted.
        let list = HandedOver::from_transfer(&handover(pages("bc|aXbc|"), "ub", Vec::new())).unwrap();
        assert_eq!((letters(&list.tracks), queued(&list)), ("abc".to_string(), "X2".to_string()));
    }

    #[test]
    fn a_handover_s_queued_songs_keep_their_place_among_the_others() {
        let list = HandedOver::from_transfer(&handover(pages("bcXdYeZ"), "uc", Vec::new())).unwrap();
        assert_eq!(letters(&list.tracks), "bcde");
        assert_eq!(queued(&list), "X0Y1Z2");
        assert_eq!((list.current, list.resume_at), (Some(1), 2));
        // One from before the song playing has played, and isn't queued again.
        let list = HandedOver::from_transfer(&handover(pages("aXbcYd"), "uc", Vec::new())).unwrap();
        assert_eq!((letters(&list.tracks), queued(&list)), ("abcd".to_string(), "Y0".to_string()));
        let list = HandedOver::from_transfer(&handover(pages("aXbcYd"), "uc", pages("XY"))).unwrap();
        assert_eq!(queued(&list), "Y0X1", "unless the queue itself says it is still to come");
    }

    #[test]
    fn a_handover_with_a_queued_song_playing() {
        let mut transfer = handover(pages("cd"), "", pages("XY"));
        transfer.queue.mut_or_insert_default().is_playing_queue = Some(true);
        let list = HandedOver::from_transfer(&transfer).unwrap();
        assert_eq!((letters(&list.tracks), queued(&list)), ("cd".to_string(), "Y0".to_string()));
        assert_eq!((list.current, list.resume_at), (None, 0));
        // The pages can begin before it. The mark is then on the song the list carries on with.
        let mut transfer = handover(pages("bcYd"), "uc", Vec::new());
        transfer.queue.mut_or_insert_default().is_playing_queue = Some(true);
        let list = HandedOver::from_transfer(&transfer).unwrap();
        assert_eq!((letters(&list.tracks), queued(&list)), ("bcd".to_string(), "Y1".to_string()));
        assert_eq!((list.current, list.resume_at), (None, 1));
    }

    #[test]
    fn a_handover_s_pages_show_how_a_repeating_list_starts() {
        let list = HandedOver::from_transfer(&handover(pages("cd|abcd|ab"), "uc", Vec::new())).unwrap();
        assert_eq!(letters(&list.tracks), "abcd");
        assert_eq!((list.current, list.resume_at), (Some(2), 3));
        // Similar songs after a list that ends are not its start.
        let mut similar = pages("cd|xy");
        for track in &mut similar[3..] {
            track.metadata.insert("provider".to_string(), "autoplay".to_string());
        }
        let list = HandedOver::from_transfer(&handover(similar, "uc", Vec::new())).unwrap();
        assert_eq!(letters(&list.tracks), "cd");
    }

    #[test]
    fn a_song_known_only_by_spotify_s_id_still_counts() {
        let id = SpotifyId::from_base62(&format!("{}a", "0".repeat(21))).unwrap();
        let by_id = ContextTrack {
            uri: Some(String::new()),
            uid: Some("ua".to_string()),
            gid: Some(id.to_raw().to_vec()),
            ..Default::default()
        };
        let list = HandedOver::from_transfer(&handover(vec![by_id, page_track('b', "context")], "ua", Vec::new())).unwrap();
        assert_eq!(letters(&list.tracks), "ab", "it gets its address from the id");
        assert_eq!((list.current, list.resume_at), (Some(0), 1));
        // So a report, which has addresses, and a handover with ids are of the same songs.
        let mut ids_only = handover(pages("cdgf"), "uc", Vec::new());
        for track in &mut ids_only.current_session.mut_or_insert_default().context.mut_or_insert_default().pages[0].tracks {
            let id = SpotifyUri::from_uri(track.uri.as_deref().unwrap()).unwrap();
            let SpotifyUri::Track { id } = id else { panic!("a track") };
            track.gid = Some(id.to_raw().to_vec());
            track.uri = Some(String::new());
            track.uid = None;
        }
        let playing = ids_only.playback.mut_or_insert_default().current_track.mut_or_insert_default();
        playing.uri = Some(uri('c'));
        let list = HandedOver::of(&ids_only, Some(&report("abd", 'c', "def")), &uri('c')).unwrap();
        assert_eq!(letters(&list.tracks), "abcdgf", "a song played that comes up again is in the list once");
    }

    #[test]
    fn the_report_is_used_when_it_is_of_the_same_song() {
        let transfer = handover(pages("cd"), "uc", Vec::new());
        let of = |transfer: &TransferState, report: Option<&PlayerState>| HandedOver::of(transfer, report, &uri('c')).unwrap();
        let same = report("ab", 'c', "def");
        assert_eq!(letters(&of(&transfer, Some(&same)).tracks), "abcdef", "it has more of the list on both sides");
        let another = report("x", 'y', "z");
        assert_eq!(letters(&of(&transfer, Some(&another)).tracks), "cd", "of another song");
        assert_eq!(letters(&of(&transfer, None).tracks), "cd", "no report");
        // A handover with only the song in it says nothing about the rest.
        let bare = handover(pages("c"), "uc", Vec::new());
        assert_eq!(letters(&of(&bare, Some(&same)).tracks), "abcdef");
    }

    #[test]
    fn a_queue_changed_just_before_the_switch_is_the_handover_s() {
        let of = |transfer: &TransferState, report: &PlayerState| HandedOver::of(transfer, Some(report), &uri('c')).unwrap();
        // The report is from before the change. Its songs played are still right.
        let list = of(&handover(pages("cdgf"), "uc", Vec::new()), &report("ab", 'c', "def"));
        assert_eq!(letters(&list.tracks), "abcdgf", "a song further down was swapped");
        assert_eq!((list.current, list.resume_at), (Some(2), 3));
        let list = of(&handover(pages("cXde"), "uc", Vec::new()), &report("ab", 'c', "de"));
        assert_eq!((letters(&list.tracks), queued(&list)), ("abcde".to_string(), "X0".to_string()), "a song was queued");
        let list = of(&handover(pages("cde"), "uc", Vec::new()), &report("ab", 'c', "d"));
        assert_eq!(letters(&list.tracks), "abcde", "the report is short of a song");
        let list = of(&handover(pages("cad"), "uc", Vec::new()), &report("ab", 'c', "d"));
        assert_eq!(letters(&list.tracks), "bcad", "a song played is coming up again: it is in the list once");
        // The same songs in the same places: the report, which has more.
        let list = of(&handover(pages("cXdYe"), "uc", Vec::new()), &report("ab", 'c', "XdYef"));
        assert_eq!((letters(&list.tracks), queued(&list)), ("abcdef".to_string(), "X0Y1".to_string()));
    }

    #[test]
    fn only_real_lists_are_asked_for() {
        assert!(HandedOver::can_be_asked_for("spotify:playlist:37i9dQZF1E38hYajo1J1J7"));
        assert!(HandedOver::can_be_asked_for("spotify:album:5LFzwirfFwBKXJQGfwmiMY"));
        for made_up in [
            "",
            "-",
            "spotify:web-api",
            "spotify:webapi",
            "spotify:webapi:3W3wJolupcEgGSI9xrX1fC",
            "spotify:unknown",
            "spotify:search:some+words",
        ] {
            assert!(!HandedOver::can_be_asked_for(made_up), "{made_up}");
        }
    }

    fn state() -> ConnectState {
        let mut state = ConnectState::default();
        state.request.device = MessageField::some(Device {
            player_state: MessageField::some(PlayerState {
                options: MessageField::some(ContextPlayerOptions::new()),
                ..Default::default()
            }),
            ..Default::default()
        });
        state
    }

    /// `state` with `list` loaded as its music and carried on from.
    fn carry_on(mut state: ConnectState, list: &HandedOver) -> ConnectState {
        let context = Context {
            uri: Some("spotify:web-api".to_string()),
            pages: vec![ContextPage::from(list.tracks.clone())],
            ..Default::default()
        };
        state.update_context(context, ContextType::Default).unwrap();
        state.carry_on_from(list).unwrap();
        state
    }

    fn played(tracks: &[ProvidedTrack]) -> String {
        tracks
            .iter()
            .map(|track| if is_marker(track) { '|' } else { track.uri.chars().last().unwrap_or('?') })
            .collect()
    }

    #[test]
    fn the_music_carries_on_with_the_list_around_the_song() {
        let list = HandedOver::from_player(&report("ab", 'c', "XdYe")).unwrap();
        let state = carry_on(state(), &list);
        assert_eq!(state.current_track(|track| track.uri.clone()), uri('c'));
        assert_eq!(played(state.prev_tracks()), "ab");
        assert_eq!(played(state.next_tracks()), "XdYe", "queued songs where they were among the others");
        let queued: Vec<bool> = state.next_tracks().iter().map(|track| track.is_queue()).collect();
        assert_eq!(queued, [true, false, true, false]);
        assert_eq!(state.player().index.track, 2);
    }

    #[test]
    fn a_handover_goes_through_the_player_s_own_first_step() {
        // As the player does it: the list is read, loaded, the handover's settings are taken
        // (which empties its pages) and then the list is carried on from.
        let mut transfer = handover(pages("cXde"), "uc", Vec::new());
        transfer.options.mut_or_insert_default().repeating_context = Some(true);
        let mut state = state();
        // A queued song from whatever played here before.
        state.add_to_queue(provided('Q', Provider::Queue), false);

        let list = HandedOver::of(&transfer, None, &uri('c')).unwrap();
        let context = Context {
            uri: Some("spotify:web-api".to_string()),
            pages: vec![ContextPage::from(list.tracks.clone())],
            ..Default::default()
        };
        state.update_context(context, ContextType::Default).unwrap();
        state.handle_initial_transfer(&mut transfer, Some("spotify:web-api".to_string()));
        state.carry_on_from(&list).unwrap();

        assert_eq!(state.current_track(|track| track.uri.clone()), uri('c'));
        assert!(state.repeat_context(), "repeat came over");
        assert!(played(state.next_tracks()).starts_with("Xde|cde|"), "{}", played(state.next_tracks()));
    }

    #[test]
    fn queued_songs_go_back_in_the_order_they_had() {
        let list = HandedOver::from_player(&report("a", 'b', "XYcZdW")).unwrap();
        assert_eq!(played(carry_on(state(), &list).next_tracks()), "XYcZdW");
        let list = HandedOver::from_transfer(&handover(pages("bXYcZdW"), "ub", Vec::new())).unwrap();
        assert_eq!(played(carry_on(state(), &list).next_tracks()), "XYcZdW");
    }

    #[test]
    fn a_queued_song_playing_is_followed_by_the_rest_of_the_list() {
        let list = HandedOver::from_player(&report("ab", 'X', "cd")).unwrap();
        let mut state = state();
        state.set_track(provided('X', Provider::Queue));
        let state = carry_on(state, &list);
        assert_eq!(state.current_track(|track| track.uri.clone()), uri('X'));
        assert_eq!(played(state.prev_tracks()), "ab");
        assert_eq!(played(state.next_tracks()), "cd");
    }

    #[test]
    fn a_repeating_list_goes_round_again_here_too() {
        let list = HandedOver::from_player(&report("bcdefghijk", 'l', "|abcdefghijkl|abc")).unwrap();
        let mut repeating = state();
        repeating.set_repeat_context(true);
        let state = carry_on(repeating, &list);
        assert_eq!(state.current_track(|track| track.uri.clone()), uri('l'));
        assert_eq!(played(state.prev_tracks()), "bcdefghijk");
        assert!(played(state.next_tracks()).starts_with("|abcdefghijkl|abc"));
    }

    #[test]
    fn a_shuffled_list_keeps_the_order_it_was_dealt() {
        let list = HandedOver::from_player(&report("q", 'c', "zam")).unwrap();
        let mut shuffled = state();
        shuffled.set_shuffle(true);
        let state = carry_on(shuffled, &list);
        assert_eq!(played(state.next_tracks()), "zam");
        assert!(state.shuffling_context(), "and still says it's shuffled");
    }

    #[test]
    fn a_long_list_loses_no_song_to_the_queued_ones() {
        // More songs than a device keeps as coming up, and two queued ones among them.
        let numbered = |number: usize| ContextTrack {
            uri: Some(format!("spotify:track:{number:022}")),
            uid: Some(format!("u{number}")),
            ..Default::default()
        };
        let list = HandedOver {
            tracks: (0..120).map(numbered).collect(),
            current: Some(0),
            resume_at: 1,
            queued: vec![Queued { after: 0, track: numbered(500) }, Queued { after: 3, track: numbered(501) }],
            read_from: THE_REPORT,
        };
        let mut state = carry_on(state(), &list);
        assert_eq!(state.next_tracks().len(), SPOTIFY_MAX_NEXT_TRACKS_SIZE);
        let mut heard = Vec::new();
        while let Some(next) = state.next_tracks().first().map(|track| track.uri.clone()) {
            heard.push(next);
            state.next_track().unwrap();
        }
        let mut expected: Vec<String> = (1..120).map(|number| numbered(number).uri.unwrap()).collect();
        expected.insert(0, numbered(500).uri.unwrap());
        expected.insert(4, numbered(501).uri.unwrap());
        assert_eq!(heard, expected);
    }

    #[test]
    fn a_handed_over_list_can_be_handed_on() {
        // What this player then reports is what the next device reads.
        let list = HandedOver::from_player(&report("ab", 'c', "XdYe")).unwrap();
        let state = carry_on(state(), &list);
        let again = HandedOver::from_player(state.player()).unwrap();
        assert_eq!(letters(&again.tracks), "abcde");
        assert_eq!(queued(&again), "X0Y1");
        assert_eq!((again.current, again.resume_at), (Some(2), 3));
    }

    #[test]
    fn a_queued_song_playing_in_both_the_report_and_the_handover() {
        let mut transfer = handover(pages("cd"), "uc", pages("XY"));
        transfer.queue.mut_or_insert_default().is_playing_queue = Some(true);
        let list = HandedOver::of(&transfer, Some(&report("ab", 'X', "Ycd")), &uri('X')).unwrap();
        assert_eq!((letters(&list.tracks), queued(&list)), ("abcd".to_string(), "Y0".to_string()));
        assert_eq!((list.current, list.resume_at), (None, 2));
        // They differ: the handover's, as it is. Neither has a song of the list playing.
        let list = HandedOver::of(&transfer, Some(&report("ab", 'X', "cd")), &uri('X')).unwrap();
        assert_eq!((letters(&list.tracks), queued(&list)), ("cd".to_string(), "Y0".to_string()));
    }

    #[test]
    fn a_list_that_doesn_t_fit_what_was_loaded_is_an_error_not_a_crash() {
        let mut list = HandedOver::from_player(&report("ab", 'c', "de")).unwrap();
        let mut state = state();
        let context = Context {
            uri: Some("spotify:web-api".to_string()),
            pages: vec![ContextPage::from(list.tracks[..2].to_vec())],
            ..Default::default()
        };
        state.update_context(context, ContextType::Default).unwrap();
        assert!(state.carry_on_from(&list).is_err(), "the song playing isn't in it");
        list.current = None;
        list.resume_at = 9;
        assert!(state.carry_on_from(&list).is_ok(), "nothing left of it to play");
        assert!(state.next_tracks().is_empty());
    }

    #[test]
    fn a_short_repeating_list_with_a_queued_song_plays_round_and_round() {
        let list = HandedOver::from_player(&report("", 'a', "bXc|abc")).unwrap();
        let mut repeating = state();
        repeating.set_repeat_context(true);
        let mut state = carry_on(repeating, &list);
        let mut heard = String::new();
        for _ in 0..8 {
            state.next_track().unwrap();
            heard.push_str(&played(std::slice::from_ref(state.player().track.as_ref().unwrap())));
        }
        assert_eq!(heard, "bXcabcab", "the queued song once, the list for ever");
    }
}
