use std::collections::HashMap;

use ckb_network::PeerIndex;
use ckb_types::{
    core::{BlockNumber, HeaderView},
    packed,
    utilities::merkle_mountain_range::VerifiableHeader,
    U256,
};
use faketime::unix_time_as_millis;

#[derive(Default, Clone)]
pub struct Peers {
    inner: HashMap<PeerIndex, Peer>,
}

#[derive(Default, Clone)]
pub struct Peer {
    // The peer is just discovered when it's `None`.
    state: PeerState,
    update_timestamp: u64,
}

#[derive(Default, Clone)]
pub(crate) struct PeerState {
    prove_request: Option<ProveRequest>,
    prove_state: Option<ProveState>,
}

#[derive(Clone)]
pub(crate) struct ProveRequest {
    mmr_activated_number: BlockNumber,
    last_header: VerifiableHeader,
    total_difficulty: U256,

    request: packed::GetBlockProof,

    skip_check_tau: bool,
}

#[derive(Clone)]
pub(crate) struct ProveState {
    mmr_activated_number: BlockNumber,
    last_header: VerifiableHeader,
    total_difficulty: U256,

    last_headers: Vec<HeaderView>,
}

impl ProveRequest {
    pub(crate) fn new(
        mmr_activated_number: BlockNumber,
        last_header: VerifiableHeader,
        total_difficulty: U256,
        request: packed::GetBlockProof,
    ) -> Self {
        Self {
            mmr_activated_number,
            last_header,
            total_difficulty,
            request,
            skip_check_tau: false,
        }
    }

    pub(crate) fn get_mmr_activated_number(&self) -> BlockNumber {
        self.mmr_activated_number
    }

    pub(crate) fn get_last_header(&self) -> &VerifiableHeader {
        &self.last_header
    }

    pub(crate) fn get_total_difficulty(&self) -> &U256 {
        &self.total_difficulty
    }

    pub(crate) fn is_same_as(
        &self,
        mmr_activated_number: BlockNumber,
        last_header: &VerifiableHeader,
        total_difficulty: &U256,
    ) -> bool {
        self.get_mmr_activated_number() == mmr_activated_number
            && self.get_last_header() == last_header
            && self.get_total_difficulty() == total_difficulty
    }

    pub(crate) fn get_request(&self) -> &packed::GetBlockProof {
        &self.request
    }

    pub(crate) fn if_skip_check_tau(&self) -> bool {
        self.skip_check_tau
    }

    pub(crate) fn skip_check_tau(&mut self) {
        self.skip_check_tau = true;
    }
}

impl ProveState {
    pub(crate) fn new_from_request(request: ProveRequest, last_headers: Vec<HeaderView>) -> Self {
        let ProveRequest {
            mmr_activated_number,
            last_header,
            total_difficulty,
            ..
        } = request;
        Self {
            mmr_activated_number,
            last_header,
            total_difficulty,
            last_headers,
        }
    }

    pub(crate) fn get_mmr_activated_number(&self) -> BlockNumber {
        self.mmr_activated_number
    }

    pub(crate) fn get_last_header(&self) -> &VerifiableHeader {
        &self.last_header
    }

    pub(crate) fn get_total_difficulty(&self) -> &U256 {
        &self.total_difficulty
    }

    pub(crate) fn is_same_as(
        &self,
        mmr_activated_number: BlockNumber,
        last_header: &VerifiableHeader,
        total_difficulty: &U256,
    ) -> bool {
        self.get_mmr_activated_number() == mmr_activated_number
            && self.get_last_header() == last_header
            && self.get_total_difficulty() == total_difficulty
    }

    pub(crate) fn get_last_headers(&self) -> &[HeaderView] {
        &self.last_headers[..]
    }
}

impl PeerState {
    pub(crate) fn is_ready(&self) -> bool {
        self.prove_request.is_some() || self.prove_state.is_some()
    }

    pub(crate) fn get_prove_request(&self) -> Option<&ProveRequest> {
        self.prove_request.as_ref()
    }

    pub(crate) fn get_prove_state(&self) -> Option<&ProveState> {
        self.prove_state.as_ref()
    }

    fn submit_prove_request(&mut self, request: ProveRequest) {
        self.prove_request = Some(request);
    }

    fn commit_prove_state(&mut self, state: ProveState) {
        self.prove_state = Some(state);
        self.prove_request = None;
    }
}

impl Peer {
    fn new(update_timestamp: u64) -> Self {
        Self {
            state: Default::default(),
            update_timestamp,
        }
    }
}

impl Peers {
    pub(crate) fn add_peer(&mut self, index: PeerIndex) {
        let now = unix_time_as_millis();
        let peer = Peer::new(now);
        self.inner.insert(index, peer);
    }

    pub(crate) fn remove_peer(&mut self, index: PeerIndex) {
        self.inner.remove(&index);
    }

    pub(crate) fn get_state(&self, index: &PeerIndex) -> Option<&PeerState> {
        self.inner.get(&index).map(|peer| &peer.state)
    }

    pub(crate) fn update_timestamp(&mut self, index: PeerIndex, timestamp: u64) {
        if let Some(peer) = self.inner.get_mut(&index) {
            peer.update_timestamp = timestamp;
        }
    }

    pub(crate) fn submit_prove_request(&mut self, index: PeerIndex, request: ProveRequest) {
        let now = unix_time_as_millis();
        if let Some(peer) = self.inner.get_mut(&index) {
            peer.state.submit_prove_request(request);
            peer.update_timestamp = now;
        }
    }

    pub(crate) fn commit_prove_state(&mut self, index: PeerIndex, state: ProveState) {
        let now = unix_time_as_millis();
        if let Some(peer) = self.inner.get_mut(&index) {
            peer.state.commit_prove_state(state);
            peer.update_timestamp = now;
        }
    }

    pub(crate) fn get_peers_which_require_updating(&self, before_timestamp: u64) -> Vec<PeerIndex> {
        self.inner
            .iter()
            .filter_map(|(index, peer)| {
                if !peer.state.is_ready() || peer.update_timestamp < before_timestamp {
                    Some(*index)
                } else {
                    None
                }
            })
            .collect()
    }

    pub(crate) fn get_peers_which_are_proved(&self) -> Vec<(PeerIndex, ProveState)> {
        self.inner
            .iter()
            .filter_map(|(index, peer)| {
                if let Some(state) = peer.state.get_prove_state() {
                    Some((*index, state.to_owned()))
                } else {
                    None
                }
            })
            .collect()
    }
}
