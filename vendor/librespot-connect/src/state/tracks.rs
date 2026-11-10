use crate::{
    core::{Error, SpotifyUri},
    protocol::player::ProvidedTrack,
    state::{
        ConnectState, SPOTIFY_MAX_NEXT_TRACKS_SIZE, SPOTIFY_MAX_PREV_TRACKS_SIZE, StateError,
        context::ContextType,
        metadata::Metadata,
        provider::{IsProvider, Provider},
    },
};
use protobuf::MessageField;
use rand::Rng;

// identifier used as part of the uid
pub const IDENTIFIER_DELIMITER: &str = "delimiter";

impl<'ct> ConnectState {
    fn new_delimiter(iteration: i64) -> ProvidedTrack {
        let mut delimiter = ProvidedTrack {
            uri: format!("spotify:{IDENTIFIER_DELIMITER}"),
            uid: format!("{IDENTIFIER_DELIMITER}{iteration}"),
            provider: Provider::Context.to_string(),
            ..Default::default()
        };
        delimiter.set_hidden(true);
        delimiter.set_iteration(iteration);

        delimiter
    }

    fn push_prev(&mut self, prev: ProvidedTrack) {
        let prev_tracks = self.prev_tracks_mut();
        // add prev track, while preserving a length of 10
        if prev_tracks.len() >= SPOTIFY_MAX_PREV_TRACKS_SIZE {
            // todo: O(n), but technically only maximal O(SPOTIFY_MAX_PREV_TRACKS_SIZE) aka O(10)
            let _ = prev_tracks.remove(0);
        }
        prev_tracks.push(prev)
    }

    fn get_next_track(&mut self) -> Option<ProvidedTrack> {
        if self.next_tracks().is_empty() {
            None
        } else {
            // todo: O(n), but technically only maximal O(SPOTIFY_MAX_NEXT_TRACKS_SIZE) aka O(80)
            Some(self.next_tracks_mut().remove(0))
        }
    }

    /// bottom => top, aka the last track of the list is the prev track
    fn prev_tracks_mut(&mut self) -> &mut Vec<ProvidedTrack> {
        &mut self.player_mut().prev_tracks
    }

    /// bottom => top, aka the last track of the list is the prev track
    pub(super) fn prev_tracks(&self) -> &Vec<ProvidedTrack> {
        &self.player().prev_tracks
    }

    /// top => bottom, aka the first track of the list is the next track
    fn next_tracks_mut(&mut self) -> &mut Vec<ProvidedTrack> {
        &mut self.player_mut().next_tracks
    }

    /// top => bottom, aka the first track of the list is the next track
    pub(super) fn next_tracks(&self) -> &Vec<ProvidedTrack> {
        &self.player().next_tracks
    }

    pub fn set_current_track_random(&mut self) -> Result<(), Error> {
        let max_tracks = self.get_context(self.active_context)?.tracks.len();
        let rng_track = rand::rng().random_range(0..max_tracks);
        self.set_current_track(rng_track)
    }

    pub fn set_current_track(&mut self, index: usize) -> Result<(), Error> {
        let context = self.get_context(self.active_context)?;

        let new_track = context
            .tracks
            .get(index)
            .ok_or(StateError::CanNotFindTrackInContext(
                Some(index),
                context.tracks.len(),
            ))?;

        debug!(
            "set track to: {} at {} of {} tracks",
            new_track.uri,
            index,
            context.tracks.len()
        );

        self.set_track(new_track.clone());

        self.update_current_index(|i| i.track = index as u32);

        Ok(())
    }

    /// Move to the next track
    ///
    /// Updates the current track to the next track. Adds the old track
    /// to prev tracks and fills up the next tracks from the current context
    pub fn next_track(&mut self) -> Result<Option<u32>, Error> {
        // when we skip in repeat track, we don't repeat the current track anymore
        if self.repeat_track() {
            self.set_repeat_track(false);
        }

        let old_track = self.player_mut().track.take();

        if let Some(old_track) = old_track {
            // only add songs from our context to our previous tracks
            if old_track.is_context() || old_track.is_autoplay() {
                self.push_prev(old_track)
            }
        }

        let new_track = loop {
            match self.get_next_track() {
                Some(next) if next.uid.starts_with(IDENTIFIER_DELIMITER) => {
                    self.push_prev(next);
                    continue;
                }
                Some(next) if next.is_unavailable() => continue,
                other => break other,
            };
        };

        let new_track = match new_track {
            None => return Ok(None),
            Some(t) => t,
        };

        self.fill_up_next_tracks()?;

        let update_index = if new_track.is_queue() {
            None
        } else if new_track.is_autoplay() {
            self.set_active_context(ContextType::Autoplay);
            None
        } else {
            match new_track.get_context_index() {
                Some(new_index) => Some(new_index as u32),
                None => {
                    error!("the given context track had no set context_index");
                    None
                }
            }
        };

        if let Some(update_index) = update_index {
            self.update_current_index(|i| i.track = update_index)
        } else {
            self.player_mut().index.clear()
        }

        self.set_track(new_track);
        self.update_restrictions();

        Ok(Some(self.player().index.track))
    }

    /// Move to the prev track
    ///
    /// Updates the current track to the prev track. Adds the old track
    /// to next tracks (when from the context) and fills up the prev tracks from the
    /// current context
    pub fn prev_track(&mut self) -> Result<Option<&MessageField<ProvidedTrack>>, Error> {
        let old_track = self.player_mut().track.take();

        if let Some(old_track) = old_track {
            if old_track.is_context() || old_track.is_autoplay() {
                // todo: O(n)
                self.next_tracks_mut().insert(0, old_track);
            }
        }

        // handle possible delimiter
        if matches!(self.prev_tracks().last(), Some(prev) if prev.uid.starts_with(IDENTIFIER_DELIMITER))
        {
            let delimiter = self
                .prev_tracks_mut()
                .pop()
                .expect("item that was prechecked");

            let next_tracks = self.next_tracks_mut();
            if next_tracks.len() >= SPOTIFY_MAX_NEXT_TRACKS_SIZE {
                let _ = next_tracks.pop();
            }
            // todo: O(n)
            next_tracks.insert(0, delimiter)
        }

        while self.next_tracks().len() > SPOTIFY_MAX_NEXT_TRACKS_SIZE {
            let _ = self.next_tracks_mut().pop();
        }

        let new_track = match self.prev_tracks_mut().pop() {
            None => return Ok(None),
            Some(t) => t,
        };

        if matches!(self.active_context, ContextType::Autoplay if new_track.is_context()) {
            // transition back to default context
            self.set_active_context(ContextType::Default);
        }

        self.fill_up_next_tracks()?;
        self.set_track(new_track);

        if self.player().index.track == 0 {
            warn!("prev: trying to skip into negative, index update skipped")
        } else {
            self.update_current_index(|i| i.track -= 1)
        }

        self.update_restrictions();

        Ok(Some(self.current_track(|t| t)))
    }

    pub fn current_track<F: Fn(&'ct MessageField<ProvidedTrack>) -> R, R>(
        &'ct self,
        access: F,
    ) -> R {
        access(&self.player().track)
    }

    pub fn set_track(&mut self, track: ProvidedTrack) {
        self.player_mut().track = MessageField::some(track)
    }

    pub fn set_next_tracks(&mut self, mut tracks: Vec<ProvidedTrack>) {
        // mobile only sends a set_queue command instead of an add_to_queue command
        // in addition to handling the mobile add_to_queue handling, this should also handle
        // a mass queue addition
        tracks
            .iter_mut()
            .filter(|t| t.is_from_queue())
            .for_each(|t| {
                t.set_provider(Provider::Queue);
                // technically we could preserve the queue-uid here,
                // but it seems to work without that, so we just override it
                t.uid = format!("q{}", self.queue_count);
                self.queue_count += 1;
            });

        // when you drag 'n drop the current track in the queue view into the "Next from: ..."
        // section, it is only send as an empty item with just the provider and metadata, so we have
        // to provide set the uri from the current track manually
        tracks
            .iter_mut()
            .filter(|t| t.uri.is_empty())
            .for_each(|t| t.uri = self.current_track(|ct| ct.uri.clone()));

        self.player_mut().next_tracks = tracks;
    }

    pub fn set_prev_tracks(&mut self, tracks: Vec<ProvidedTrack>) {
        self.player_mut().prev_tracks = tracks;
    }

    pub fn clear_prev_track(&mut self) {
        self.prev_tracks_mut().clear()
    }

    pub fn clear_next_tracks(&mut self) {
        // respect queued track and don't throw them out of our next played tracks
        let first_non_queued_track = self
            .next_tracks()
            .iter()
            .enumerate()
            .find(|(_, track)| !track.is_queue());

        if let Some((non_queued_track, _)) = first_non_queued_track {
            while self.next_tracks().len() > non_queued_track
                && self.next_tracks_mut().pop().is_some()
            {}
        }
    }

    pub fn fill_up_next_tracks(&mut self) -> Result<(), Error> {
        let ctx = self.get_context(self.fill_up_context)?;
        let mut new_index = ctx.index.track as usize;
        let mut iteration = ctx.index.page;

        while self.next_tracks().len() < SPOTIFY_MAX_NEXT_TRACKS_SIZE {
            let ctx = self.get_context(self.fill_up_context)?;
            let track = match ctx.tracks.get(new_index) {
                None if self.repeat_context() => {
                    let delimiter = Self::new_delimiter(iteration.into());
                    iteration += 1;
                    new_index = 0;
                    delimiter
                }
                None if !matches!(self.fill_up_context, ContextType::Autoplay)
                    && self.autoplay_context.is_some()
                    && !self.repeat_context() =>
                {
                    self.update_context_index(self.fill_up_context, new_index)?;

                    // transition to autoplay as fill up context
                    self.fill_up_context = ContextType::Autoplay;
                    new_index = self.get_context(ContextType::Autoplay)?.index.track as usize;

                    // add delimiter to only display the current context
                    Self::new_delimiter(iteration.into())
                }
                None if self.autoplay_context.is_some() => {
                    match self
                        .get_context(ContextType::Autoplay)?
                        .tracks
                        .get(new_index)
                    {
                        None => break,
                        Some(ct) => {
                            new_index += 1;
                            ct.clone()
                        }
                    }
                }
                None => break,
                Some(ct) if ct.is_unavailable() || self.is_skip_track(ct, Some(iteration)) => {
                    debug!(
                        "skipped track {} during fillup as it's unavailable or should be skipped",
                        ct.uri
                    );
                    new_index += 1;
                    continue;
                }
                Some(ct) => {
                    new_index += 1;
                    ct.clone()
                }
            };

            self.next_tracks_mut().push(track);
        }

        debug!(
            "finished filling up next_tracks ({})",
            self.next_tracks().len()
        );

        self.update_context_index(self.fill_up_context, new_index)?;

        // the web-player needs a revision update, otherwise the queue isn't updated in the ui
        self.update_queue_revision();

        Ok(())
    }

    pub fn preview_next_track(&mut self) -> Option<SpotifyUri> {
        let next = if self.repeat_track() {
            self.current_track(|t| &t.uri)
        } else {
            &self.next_tracks().first()?.uri
        };

        SpotifyUri::from_uri(next).ok()
    }

    pub fn has_next_tracks(&self, min: Option<usize>) -> bool {
        if let Some(min) = min {
            self.next_tracks().len() >= min
        } else {
            !self.next_tracks().is_empty()
        }
    }

    pub fn recent_track_uris(&self) -> Vec<String> {
        let mut prev = self
            .prev_tracks()
            .iter()
            .map(|t| t.uri.clone())
            .collect::<Vec<_>>();

        prev.push(self.current_track(|t| t.uri.clone()));
        prev
    }

    pub fn mark_unavailable(&mut self, id: &SpotifyUri) -> Result<(), Error> {
        let uri = id.to_uri()?;

        debug!("marking {uri} as unavailable");

        let next_tracks = self.next_tracks_mut();
        while let Some(pos) = next_tracks.iter().position(|t| t.uri == uri) {
            let _ = next_tracks.remove(pos);
        }

        for next_track in next_tracks {
            Self::mark_as_unavailable_for_match(next_track, &uri)
        }

        let prev_tracks = self.prev_tracks_mut();
        while let Some(pos) = prev_tracks.iter().position(|t| t.uri == uri) {
            let _ = prev_tracks.remove(pos);
        }

        for prev_track in prev_tracks {
            Self::mark_as_unavailable_for_match(prev_track, &uri)
        }

        self.unavailable_uri.push(uri);
        self.fill_up_next_tracks()?;
        self.update_queue_revision();

        Ok(())
    }

    /// Spotlight: puts the songs coming up in a new order. A song already coming up keeps
    /// its place in the playlist, so the previous button still goes back through it, and a
    /// new one joins as a queued song. Songs that were never rows stay behind the rows.
    /// Emptying the rows ends the list: nothing else from the playlist follows.
    pub fn set_upcoming(&mut self, rows_before: &[String], rows_after: &[String]) {
        let mut waiting = std::mem::take(self.next_tracks_mut());
        let mut upcoming = Vec::with_capacity(rows_after.len() + waiting.len());
        for uri in rows_after {
            match waiting.iter().position(|track| &track.uri == uri) {
                Some(at) => upcoming.push(waiting.remove(at)),
                None => {
                    let mut track = ProvidedTrack {
                        uri: uri.clone(),
                        uid: format!("q{}", self.queue_count),
                        ..Default::default()
                    };
                    self.queue_count += 1;
                    track.set_provider(Provider::Queue);
                    track.set_from_queue(true);
                    upcoming.push(track);
                }
            }
        }

        if rows_after.is_empty() {
            let end = self
                .get_context(self.fill_up_context)
                .map(|context| context.tracks.len());
            if let Ok(end) = end {
                let _ = self.update_context_index(self.fill_up_context, end);
            }
        } else {
            waiting.retain(|track| !rows_before.contains(&track.uri));
            upcoming.append(&mut waiting);
        }

        let pushed_off = upcoming.split_off(SPOTIFY_MAX_NEXT_TRACKS_SIZE.min(upcoming.len()));
        self.return_to_context(&pushed_off);
        *self.next_tracks_mut() = upcoming;
        self.update_queue_revision();
        self.update_restrictions();
    }

    /// Songs that no longer fit on the list wait in their playlist again, as filling up has already
    /// gone past them and they would otherwise never play. A list running into a repeat of the
    /// playlist is left as it is, as those songs come round anyway.
    fn return_to_context(&mut self, pushed_off: &[ProvidedTrack]) {
        let all_from_context = pushed_off
            .iter()
            .all(|track| track.is_context() && !track.uid.starts_with(IDENTIFIER_DELIMITER));
        let Some(first) = pushed_off.first().filter(|_| all_from_context) else {
            return;
        };
        let at = self
            .get_context(self.fill_up_context)
            .ok()
            .and_then(|context| context.tracks.iter().position(|track| track.uid == first.uid));
        if let Some(at) = at {
            let _ = self.update_context_index(self.fill_up_context, at);
        }
    }

    pub fn add_to_queue(&mut self, mut track: ProvidedTrack, rev_update: bool) {
        track.uid = format!("q{}", self.queue_count);
        self.queue_count += 1;

        track.set_provider(Provider::Queue);
        if !track.is_from_queue() {
            track.set_from_queue(true);
        }

        let next_tracks = self.next_tracks_mut();
        if let Some(next_not_queued_track) = next_tracks.iter().position(|t| !t.is_queue()) {
            next_tracks.insert(next_not_queued_track, track);
        } else {
            next_tracks.push(track)
        }

        while next_tracks.len() > SPOTIFY_MAX_NEXT_TRACKS_SIZE {
            next_tracks.pop();
        }

        if rev_update {
            self.update_queue_revision();
        }
        self.update_restrictions();
    }
}

#[cfg(test)]
mod set_upcoming_tests {
    use super::*;
    use crate::{
        protocol::{
            connect::Device,
            player::{ContextIndex, PlayerState},
        },
        state::context::StateContext,
    };

    fn state_with(next: &[(&str, Provider)]) -> ConnectState {
        let mut state = ConnectState::default();
        state.request.device = MessageField::some(Device {
            player_state: MessageField::some(PlayerState::default()),
            ..Default::default()
        });
        state.fill_up_context = ContextType::Default;
        *state.next_tracks_mut() = next
            .iter()
            .enumerate()
            .map(|(index, (uri, provider))| {
                let mut track = ProvidedTrack {
                    uri: uri.to_string(),
                    uid: format!("u{index}"),
                    ..Default::default()
                };
                track.set_provider(provider.clone());
                track
            })
            .collect();
        state
    }

    fn uris(state: &ConnectState) -> Vec<&str> {
        state.next_tracks().iter().map(|track| track.uri.as_str()).collect()
    }

    fn list(uris: &[&str]) -> Vec<String> {
        uris.iter().map(|uri| uri.to_string()).collect()
    }

    fn context(uris: &[&str]) -> Vec<(String, Provider)> {
        uris.iter().map(|uri| (uri.to_string(), Provider::Context)).collect()
    }

    fn state_of(uris: &[&str]) -> ConnectState {
        let tracks = context(uris);
        let tracks: Vec<(&str, Provider)> =
            tracks.iter().map(|(uri, provider)| (uri.as_str(), provider.clone())).collect();
        state_with(&tracks)
    }

    #[test]
    fn a_moved_song_keeps_what_it_was() {
        let mut state = state_of(&["a", "b", "c"]);
        let before = state.next_tracks()[2].clone();
        state.set_upcoming(&list(&["a", "b", "c"]), &list(&["c", "a", "b"]));
        assert_eq!(uris(&state), ["c", "a", "b"]);
        assert_eq!(state.next_tracks()[0], before, "still from the playlist, with its own id");
        assert!(state.next_tracks()[0].is_context());
    }

    #[test]
    fn songs_past_the_rows_stay_behind_them() {
        let mut state = state_of(&["a", "b", "c", "d", "e"]);
        state.set_upcoming(&list(&["a", "b", "c"]), &list(&["b", "a", "c"]));
        assert_eq!(uris(&state), ["b", "a", "c", "d", "e"]);
    }

    #[test]
    fn a_removed_row_is_gone() {
        let mut state = state_of(&["a", "b", "c", "d"]);
        state.set_upcoming(&list(&["a", "b", "c"]), &list(&["a", "c"]));
        assert_eq!(uris(&state), ["a", "c", "d"]);
    }

    #[test]
    fn a_removed_copy_of_a_song_goes_when_the_other_stays() {
        let mut state = state_of(&["a", "b", "a"]);
        state.set_upcoming(&list(&["a", "b", "a"]), &list(&["b", "a"]));
        assert_eq!(uris(&state), ["b", "a"]);
    }

    #[test]
    fn a_new_song_joins_as_queued() {
        let mut state = state_of(&["a", "b"]);
        state.set_upcoming(&list(&["a", "b"]), &list(&["n", "a", "b"]));
        assert_eq!(uris(&state), ["n", "a", "b"]);
        let new = &state.next_tracks()[0];
        assert!(new.is_queue() && new.is_from_queue());
        assert_eq!(new.uid, "q0");
        state.set_upcoming(&list(&["n", "a", "b"]), &list(&["m", "n", "a", "b"]));
        assert_eq!(state.next_tracks()[0].uid, "q1", "each gets its own id");
    }

    #[test]
    fn a_list_the_rows_dont_match_is_kept_behind_them() {
        let mut state = state_of(&["x", "y"]);
        state.set_upcoming(&list(&["a"]), &list(&["a", "b"]));
        assert_eq!(uris(&state), ["a", "b", "x", "y"]);
    }

    #[test]
    fn emptying_the_rows_leaves_nothing_coming_up() {
        let mut state = state_of(&["a", "b", "c", "d"]);
        state.set_upcoming(&list(&["a", "b"]), &[]);
        assert!(state.next_tracks().is_empty());
    }

    #[test]
    fn the_list_stays_within_what_spotify_accepts() {
        let names: Vec<String> = (0..SPOTIFY_MAX_NEXT_TRACKS_SIZE + 5).map(|n| format!("s{n}")).collect();
        let mut state = state_of(&[]);
        state.set_upcoming(&[], &names);
        assert_eq!(state.next_tracks().len(), SPOTIFY_MAX_NEXT_TRACKS_SIZE);
    }

    /// Playing the first of these songs from a playlist.
    fn playing(playlist: &[String]) -> ConnectState {
        let mut state = state_of(&[]);
        let tracks: Vec<ProvidedTrack> = playlist
            .iter()
            .enumerate()
            .map(|(index, uri)| {
                let mut track = ProvidedTrack {
                    uri: uri.clone(),
                    uid: format!("c{index}"),
                    provider: Provider::Context.to_string(),
                    ..Default::default()
                };
                track.set_context_index(index);
                track
            })
            .collect();
        let first = tracks[0].clone();
        state.context = Some(StateContext {
            tracks: tracks.into(),
            metadata: Default::default(),
            restrictions: None,
            index: ContextIndex { track: 1, ..Default::default() },
        });
        state.set_track(first);
        state.fill_up_next_tracks().unwrap();
        state
    }

    /// Playing the first of these songs from a playlist in shuffle, always in the same order.
    fn shuffled(playlist: &[String]) -> ConnectState {
        let mut state = playing(playlist);
        state.set_shuffle(true);
        state
            .shuffle_restore(crate::state::options::ShuffleState {
                seed: 123_456_789_012,
                initial_track: playlist[0].clone(),
            })
            .unwrap();
        state
    }

    fn numbered(count: usize) -> Vec<String> {
        (0..count).map(|n| format!("s{n}")).collect()
    }

    fn play_to_the_end(state: &mut ConnectState) -> Vec<String> {
        let mut played = Vec::new();
        while state.next_track().unwrap().is_some() {
            played.push(state.current_track(|track| track.uri.clone()));
        }
        played
    }

    #[test]
    fn a_moved_song_plays_next_and_previous_goes_back_through_what_played() {
        let mut state = playing(&list(&["a", "b", "c", "d"]));
        state.set_upcoming(&list(&["b", "c", "d"]), &list(&["d", "b", "c"]));

        state.next_track().unwrap();
        assert_eq!(state.current_track(|track| track.uri.clone()), "d");
        state.next_track().unwrap();
        assert_eq!(state.current_track(|track| track.uri.clone()), "b");

        state.prev_track().unwrap();
        assert_eq!(state.current_track(|track| track.uri.clone()), "d");
        state.prev_track().unwrap();
        assert_eq!(state.current_track(|track| track.uri.clone()), "a");
    }

    #[test]
    fn a_long_playlist_carries_on_after_the_rows_with_nothing_lost_or_repeated() {
        let songs = numbered(120);
        let mut state = playing(&songs);
        let rows = songs[1..=50].to_vec();
        let mut moved = rows.clone();
        moved.reverse();
        state.set_upcoming(&rows, &moved);

        let played = play_to_the_end(&mut state);
        let mut expected = moved.clone();
        expected.extend_from_slice(&songs[51..]);
        assert_eq!(played, expected);
    }

    #[test]
    fn a_removed_song_doesnt_come_back_as_the_list_tops_itself_up() {
        let songs = numbered(120);
        let mut state = playing(&songs);
        let rows = songs[1..=50].to_vec();
        let mut kept = rows.clone();
        kept.retain(|uri| uri != "s10");
        state.set_upcoming(&rows, &kept);

        let played = play_to_the_end(&mut state);
        assert!(!played.contains(&"s10".to_string()));
        assert_eq!(played.len(), 118);
    }

    #[test]
    fn emptying_the_rows_ends_the_playlist_too() {
        let songs = numbered(120);
        let mut state = playing(&songs);
        state.set_upcoming(&songs[1..=50].to_vec(), &[]);
        assert!(play_to_the_end(&mut state).is_empty());
    }

    #[test]
    fn a_song_added_after_emptying_is_all_that_plays() {
        let songs = numbered(120);
        let mut state = playing(&songs);
        state.set_upcoming(&songs[1..=50].to_vec(), &[]);
        state.set_upcoming(&[], &list(&["n"]));
        assert_eq!(play_to_the_end(&mut state), ["n"]);
    }

    #[test]
    fn a_song_added_to_a_full_list_costs_no_other_song() {
        let songs = numbered(120);
        let mut state = playing(&songs);
        assert_eq!(state.next_tracks().len(), SPOTIFY_MAX_NEXT_TRACKS_SIZE);
        let rows = songs[1..=50].to_vec();
        let mut added = rows.clone();
        added.push("n".to_string());
        state.set_upcoming(&rows, &added);
        assert_eq!(state.next_tracks().len(), SPOTIFY_MAX_NEXT_TRACKS_SIZE);

        let mut expected = added.clone();
        expected.extend_from_slice(&songs[51..]);
        assert_eq!(play_to_the_end(&mut state), expected);
    }

    #[test]
    fn a_move_in_shuffle_leaves_the_rest_of_the_shuffle_alone() {
        let songs = numbered(120);
        let order = play_to_the_end(&mut shuffled(&songs));
        assert_eq!(order.len(), 119, "everything but the song playing");
        assert_ne!(order, songs[1..], "really shuffled");

        let mut state = shuffled(&songs);
        let rows = order[..20].to_vec();
        let mut moved = rows.clone();
        moved.rotate_right(1);
        state.set_upcoming(&rows, &moved);

        let mut expected = moved.clone();
        expected.extend_from_slice(&order[20..]);
        assert_eq!(play_to_the_end(&mut state), expected);
    }

    #[test]
    fn a_song_removed_in_shuffle_stays_gone() {
        let songs = numbered(120);
        let order = play_to_the_end(&mut shuffled(&songs));
        let mut state = shuffled(&songs);
        let rows = order[..20].to_vec();
        let gone = rows[5].clone();
        let mut kept = rows.clone();
        kept.retain(|uri| *uri != gone);
        state.set_upcoming(&rows, &kept);

        let mut expected = order.clone();
        expected.retain(|uri| *uri != gone);
        assert_eq!(play_to_the_end(&mut state), expected);
    }

    #[test]
    fn a_song_added_in_shuffle_comes_after_the_rows_and_the_shuffle_carries_on() {
        let songs = numbered(120);
        let order = play_to_the_end(&mut shuffled(&songs));
        let mut state = shuffled(&songs);
        let rows = order[..20].to_vec();
        let mut added = rows.clone();
        added.push("n".to_string());
        state.set_upcoming(&rows, &added);

        let mut expected = added.clone();
        expected.extend_from_slice(&order[20..]);
        assert_eq!(play_to_the_end(&mut state), expected);
    }

    #[test]
    fn clearing_the_rows_ends_a_shuffle_too() {
        let songs = numbered(120);
        let order = play_to_the_end(&mut shuffled(&songs));
        let mut state = shuffled(&songs);
        state.set_upcoming(&order[..20].to_vec(), &[]);
        assert!(play_to_the_end(&mut state).is_empty());
    }

    #[test]
    fn the_revision_follows_the_order() {
        let mut state = state_of(&["a", "b"]);
        state.set_upcoming(&list(&["a", "b"]), &list(&["b", "a"]));
        let first = state.player().queue_revision.clone();
        assert!(!first.is_empty());
        state.set_upcoming(&list(&["b", "a"]), &list(&["a", "b"]));
        assert_ne!(state.player().queue_revision, first);
    }
}
