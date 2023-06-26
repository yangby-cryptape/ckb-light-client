use std::{cmp::Ordering, fmt};

use ckb_constant::consensus::TAU;
use ckb_merkle_mountain_range::{leaf_index_to_mmr_size, leaf_index_to_pos};
use ckb_network::{CKBProtocolContext, PeerIndex};
use ckb_types::{
    core::{BlockNumber, EpochNumber, EpochNumberWithFraction, HeaderView},
    packed,
    prelude::*,
    utilities::{
        compact_to_difficulty,
        merkle_mountain_range::{MMRProof, VerifiableHeader},
    },
    U256,
};
use log::{debug, error, log_enabled, trace, warn, Level};

use super::super::{
    peers::ProveRequest, prelude::*, LastState, LightClientProtocol, ProveState, Status, StatusCode,
};

pub(crate) struct SendLastStateProofProcess<'a> {
    message: packed::SendLastStateProofReader<'a>,
    protocol: &'a mut LightClientProtocol,
    peer_index: PeerIndex,
    nc: &'a dyn CKBProtocolContext,
}

impl<'a> SendLastStateProofProcess<'a> {
    pub(crate) fn new(
        message: packed::SendLastStateProofReader<'a>,
        protocol: &'a mut LightClientProtocol,
        peer_index: PeerIndex,
        nc: &'a dyn CKBProtocolContext,
    ) -> Self {
        Self {
            message,
            protocol,
            peer_index,
            nc,
        }
    }

    pub(crate) fn execute(self) -> Status {
        let peer_state = return_if_failed!(self.protocol.get_peer_state(&self.peer_index));

        let last_header: VerifiableHeader = self.message.last_header().to_entity().into();

        let (original_request, is_trusted_state) = if let Some(original_request) =
            peer_state.get_prove_request()
        {
            (original_request.clone(), false)
        } else if let Some(original_request) = peer_state.get_prove_request_for_trusted_state() {
            return_if_failed!(self.protocol.check_verifiable_header(&last_header));
            if last_header.header().hash() == *original_request.get_trusted_hash() {
                (original_request.complete(last_header.clone()), true)
            } else {
                let errmsg = format!(
                    "trusted state is {:#x} but got {:#x}",
                    last_header.header().hash(),
                    original_request.get_trusted_hash()
                );
                warn!(
                    "peer {} send an untrusted state, {}",
                    self.peer_index, errmsg
                );
                return StatusCode::NotTrustedState.with_context(errmsg);
            }
        } else {
            warn!("peer {} isn't waiting for a proof", self.peer_index);
            return Status::ok();
        };

        // Update the last state if the response contains a new one.
        if !is_trusted_state && !original_request.is_same_as(&last_header) {
            if self.message.proof().is_empty() {
                return_if_failed!(self
                    .protocol
                    .process_last_state(self.peer_index, last_header));
                let is_sent =
                    return_if_failed!(self.protocol.get_last_state_proof(self.nc, self.peer_index));
                if !is_sent {
                    debug!(
                        "peer {} skip sending a request for last state proof",
                        self.peer_index
                    );
                }
            } else {
                warn!("peer {} send an unknown proof", self.peer_index);
            }
            return Status::ok();
        }

        let headers = self
            .message
            .headers()
            .iter()
            .map(|header| header.to_entity().into())
            .collect::<Vec<VerifiableHeader>>();
        let last_n_blocks = self.protocol.last_n_blocks() as usize;

        // Check if the response is match the request.
        let (reorg_count, sampled_count, last_n_count) =
            return_if_failed!(check_if_response_is_matched(
                last_n_blocks,
                original_request.get_content(),
                &headers,
                &last_header
            ));
        trace!(
            "peer {}: headers count: reorg: {}, sampled: {}, last_n: {}",
            self.peer_index,
            reorg_count,
            sampled_count,
            last_n_count
        );

        // Check chain root for all headers.
        return_if_failed!(self.protocol.check_chain_root_for_headers(headers.iter()));

        let headers = headers
            .iter()
            .map(|item| item.header().to_owned())
            .collect::<Vec<_>>();

        // Check POW for all headers.
        return_if_failed!(self.protocol.check_pow_for_headers(headers.iter()));

        // Check tau with epoch difficulties of samples.
        let failed_to_verify_tau = if original_request.if_skip_check_tau() {
            trace!(
                "peer {} skip checking TAU since the flag is set",
                self.peer_index
            );
            false
        } else if sampled_count != 0 {
            let start_header = &headers[reorg_count];
            let end_header = &headers[reorg_count + sampled_count + last_n_count - 1];
            match verify_tau(
                start_header.epoch(),
                start_header.compact_target(),
                end_header.epoch(),
                end_header.compact_target(),
                TAU,
            ) {
                Ok(result) => !result,
                Err(status) => return status,
            }
        } else {
            trace!(
                "peer {} skip checking TAU since no sampled headers",
                self.peer_index
            );
            false
        };

        // Check if headers are continuous.
        if reorg_count != 0 {
            return_if_failed!(check_continuous_headers(&headers[..reorg_count - 1]));
        }
        return_if_failed!(check_continuous_headers(
            &headers[reorg_count + sampled_count..]
        ));

        // Verify MMR proof
        return_if_failed!(verify_mmr_proof(
            self.protocol.mmr_activated_epoch(),
            &last_header,
            self.message.proof(),
            headers.iter()
        ));

        // Check total difficulty.
        //
        // If no sampled headers, we can skip the check for total difficulty
        // since POW checks with continuous checks is enough.
        if sampled_count != 0 {
            if let Some(prove_state) = peer_state.get_prove_state() {
                let prev_last_header = prove_state.get_last_header();
                let start_header = prev_last_header.header();
                let end_header = last_header.header();
                if let Err(msg) = verify_total_difficulty(
                    start_header.epoch(),
                    start_header.compact_target(),
                    &prev_last_header.total_difficulty(),
                    end_header.epoch(),
                    end_header.compact_target(),
                    &last_header.total_difficulty(),
                    TAU,
                ) {
                    return StatusCode::InvalidTotalDifficulty.with_context(msg);
                }
            }
        }

        if failed_to_verify_tau {
            // Ask for new sampled headers if all checks are passed, expect the TAU check.
            if let Some(content) = self
                .protocol
                .build_prove_request_content(&peer_state, &last_header)
            {
                let mut prove_request =
                    ProveRequest::new(LastState::new(last_header), content.clone());
                prove_request.skip_check_tau();
                return_if_failed!(self
                    .protocol
                    .peers()
                    .update_prove_request(self.peer_index, prove_request));

                let message = packed::LightClientMessage::new_builder()
                    .set(content)
                    .build();
                self.nc.reply(self.peer_index, &message);

                let errmsg = "failed to verify TAU";
                return StatusCode::RequireRecheck.with_context(errmsg);
            } else {
                warn!("peer {}, build prove request failed", self.peer_index);
            }
        } else {
            let reorg_last_headers = headers[..reorg_count]
                .iter()
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>();
            let mut new_last_headers = headers[headers.len() - last_n_count..]
                .iter()
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>();
            let last_headers = match last_n_count.cmp(&last_n_blocks) {
                Ordering::Equal => new_last_headers,
                Ordering::Greater => {
                    let split_at = last_n_count - last_n_blocks;
                    new_last_headers.split_off(split_at)
                }
                Ordering::Less => {
                    if let Some(prove_state) = peer_state.get_prove_state() {
                        let old_last_headers = if reorg_count == 0 {
                            prove_state.get_last_headers()
                        } else {
                            &headers[..reorg_count]
                        };
                        // last_headers from previous prove state are empty
                        // iff the chain only has 1 block after MMR enabled.
                        if old_last_headers.is_empty() {
                            new_last_headers
                        } else {
                            let required_count = last_n_blocks - last_n_count;
                            let old_last_headers_len = old_last_headers.len();
                            old_last_headers
                                .iter()
                                .skip(old_last_headers_len.saturating_sub(required_count))
                                .map(ToOwned::to_owned)
                                .chain(new_last_headers.into_iter())
                                .collect::<Vec<_>>()
                        }
                    } else if reorg_count == 0 {
                        new_last_headers
                    } else {
                        // If this branch is reached, the follow conditions must be satisfied:
                        // - No previous prove state.
                        // - `reorg_count > 0`
                        //
                        // If there is no previous prove state, why it requires reorg?
                        // So we consider that the peer is malicious.
                        //
                        // TODO This branch should be unreachable.
                        warn!(
                            "peer {}: no previous prove state but has reorg blocks, \
                            reorg: {reorg_count}, sampled: {sampled_count}, last_n_real: {last_n_count}, \
                            last_n_param: {last_n_blocks}, original_request: {original_request}",
                            self.peer_index,
                        );
                        let errmsg = "no previous prove state but has reorg blocks";
                        return StatusCode::InvalidReorgHeaders.with_context(errmsg);
                    }
                }
            };

            // Commit the status if all checks are passed.
            let prove_state = ProveState::new_from_request(
                original_request.to_owned(),
                reorg_last_headers,
                last_headers,
            );

            if original_request.if_long_fork_detected() {
                error!(
                    "Long fork detected, please check if ckb-light-client is connected to \
                     the same network ckb node. If you connected ckb-light-client to a dev \
                     chain for testing purpose you should remove the storage of \
                     ckb-light-client to recover."
                );
                panic!("long fork detected");
            }

            let long_fork_detected = !return_if_failed!(self
                .protocol
                .commit_prove_state(self.peer_index, prove_state.clone()));

            if long_fork_detected {
                // Should NOT reach here if the client is waiting for a trusted state proof,
                // since the start number is 0.
                assert!(!is_trusted_state);
                let last_header = prove_state.get_last_header();
                if let Some(content) = self
                    .protocol
                    .build_prove_request_content_from_genesis(last_header)
                {
                    let mut prove_request =
                        ProveRequest::new(LastState::new(last_header.clone()), content.clone());
                    prove_request.long_fork_detected();
                    return_if_failed!(self
                        .protocol
                        .peers()
                        .update_prove_request(self.peer_index, prove_request));

                    let message = packed::LightClientMessage::new_builder()
                        .set(content)
                        .build();
                    self.nc.reply(self.peer_index, &message);

                    let errmsg = "long fork detected";
                    return StatusCode::RequireRecheck.with_context(errmsg);
                } else {
                    warn!(
                        "peer {}, build prove request from genesis failed",
                        self.peer_index
                    );
                }
            }
        }

        debug!("block proof verify passed for peer: {}", self.peer_index);
        Status::ok()
    }
}

#[derive(Debug, Clone)]
pub(crate) enum EpochDifficultyTrend {
    Unchanged,
    Increased { start: U256, end: U256 },
    Decreased { start: U256, end: U256 },
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum EstimatedLimit {
    Min,
    Max,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum EpochCountGroupByTrend {
    Increased(u64),
    Decreased(u64),
}

#[derive(Debug, Clone)]
pub(crate) struct EpochDifficultyTrendDetails {
    pub(crate) start: EpochCountGroupByTrend,
    pub(crate) end: EpochCountGroupByTrend,
}

impl EpochDifficultyTrend {
    pub(crate) fn new(start_epoch_difficulty: &U256, end_epoch_difficulty: &U256) -> Self {
        match start_epoch_difficulty.cmp(end_epoch_difficulty) {
            Ordering::Equal => Self::Unchanged,
            Ordering::Less => Self::Increased {
                start: start_epoch_difficulty.clone(),
                end: end_epoch_difficulty.clone(),
            },
            Ordering::Greater => Self::Decreased {
                start: start_epoch_difficulty.clone(),
                end: end_epoch_difficulty.clone(),
            },
        }
    }

    pub(crate) fn check_tau(&self, tau: u64, epochs_switch_count: u64) -> bool {
        match self {
            Self::Unchanged => {
                trace!("end epoch difficulty is same as the start epoch",);
                true
            }
            Self::Increased { ref start, ref end } => {
                let mut end_max = start.clone();
                let tau_u256 = U256::from(tau);
                for _ in 0..epochs_switch_count {
                    end_max = end_max.saturating_mul(&tau_u256);
                }
                trace!(
                    "end epoch difficulty is {} and upper limit is {}",
                    end,
                    end_max
                );
                *end <= end_max
            }

            Self::Decreased { ref start, ref end } => {
                let mut end_min = start.clone();
                for _ in 0..epochs_switch_count {
                    end_min /= tau;
                }
                trace!(
                    "end epoch difficulty is {} and lower limit is {}",
                    end,
                    end_min
                );
                *end >= end_min
            }
        }
    }

    // Calculate the `k` which satisfied that
    // - `0 <= k < limit`;
    // - If the epoch difficulty was
    //   - unchanged: `k = 0`.
    //   - increased: `lhs * (tau ^ k) < rhs <= lhs * (tau ^ (k+1))`.
    //   - decreased: `lhs * (tau ^ (-k)) > rhs >= lhs * (tau ^ (-k-1))`.
    //
    // Ref: Page 18, 6.1 Variable Difficulty MMR in [FlyClient: Super-Light Clients for Cryptocurrencies].
    //
    // [FlyClient: Super-Light Clients for Cryptocurrencies]: https://eprint.iacr.org/2019/226.pdf
    pub(crate) fn calculate_tau_exponent(&self, tau: u64, limit: u64) -> Option<u64> {
        match self {
            Self::Unchanged => Some(0),
            Self::Increased { ref start, ref end } => {
                let mut tmp = start.clone();
                let tau_u256 = U256::from(tau);
                for k in 0..limit {
                    tmp = tmp.saturating_mul(&tau_u256);
                    if tmp >= *end {
                        return Some(k);
                    }
                }
                None
            }

            Self::Decreased { ref start, ref end } => {
                let mut tmp = start.clone();
                for k in 0..limit {
                    tmp /= tau;
                    if tmp <= *end {
                        return Some(k);
                    }
                }
                None
            }
        }
    }

    // Split the epochs into two parts base on the trend of their difficulty changed,
    // then calculate the length of each parts.
    //
    // ### Note
    //
    // - To estimate:
    //   - the minimum limit, decreasing the epoch difficulty at first, then increasing.
    //   - the maximum limit, increasing the epoch difficulty at first, then decreasing.
    //
    // - Both parts of epochs exclude the start block and the end block.
    pub(crate) fn split_epochs(
        &self,
        limit: EstimatedLimit,
        n: u64,
        k: u64,
    ) -> EpochDifficultyTrendDetails {
        let (increased, decreased) = match (limit, self) {
            (EstimatedLimit::Min, Self::Unchanged) => {
                let decreased = (n + 1) / 2;
                let increased = n - decreased;
                (increased, decreased)
            }
            (EstimatedLimit::Max, Self::Unchanged) => {
                let increased = (n + 1) / 2;
                let decreased = n - increased;
                (increased, decreased)
            }
            (EstimatedLimit::Min, Self::Increased { .. }) => {
                let decreased = (n - k + 1) / 2;
                let increased = n - decreased;
                (increased, decreased)
            }
            (EstimatedLimit::Max, Self::Increased { .. }) => {
                let increased = (n - k + 1) / 2 + k;
                let decreased = n - increased;
                (increased, decreased)
            }
            (EstimatedLimit::Min, Self::Decreased { .. }) => {
                let decreased = (n - k + 1) / 2 + k;
                let increased = n - decreased;
                (increased, decreased)
            }
            (EstimatedLimit::Max, Self::Decreased { .. }) => {
                let increased = (n - k + 1) / 2;
                let decreased = n - increased;
                (increased, decreased)
            }
        };
        match limit {
            EstimatedLimit::Min => EpochDifficultyTrendDetails {
                start: EpochCountGroupByTrend::Decreased(decreased),
                end: EpochCountGroupByTrend::Increased(increased),
            },
            EstimatedLimit::Max => EpochDifficultyTrendDetails {
                start: EpochCountGroupByTrend::Increased(increased),
                end: EpochCountGroupByTrend::Decreased(decreased),
            },
        }
    }

    // Calculate the limit of total difficulty.
    pub(crate) fn calculate_total_difficulty_limit(
        &self,
        start_epoch_difficulty: &U256,
        tau: u64,
        details: &EpochDifficultyTrendDetails,
    ) -> U256 {
        let mut curr = start_epoch_difficulty.clone();
        let mut total = U256::zero();
        let tau_u256 = U256::from(tau);
        for group in &[details.start, details.end] {
            match group {
                EpochCountGroupByTrend::Decreased(epochs_count) => {
                    let state = "decreased";
                    for index in 0..*epochs_count {
                        curr /= tau;
                        total = total.checked_add(&curr).unwrap_or_else(|| {
                            panic!(
                                "overflow when calculate the limit of total difficulty, \
                                total: {}, current: {}, index: {}/{}, tau: {}, \
                                state: {}, trend: {:?}, details: {:?}",
                                total, curr, index, epochs_count, tau, state, self, details
                            );
                        })
                    }
                }
                EpochCountGroupByTrend::Increased(epochs_count) => {
                    let state = "increased";
                    for index in 0..*epochs_count {
                        curr = curr.saturating_mul(&tau_u256);
                        total = total.checked_add(&curr).unwrap_or_else(|| {
                            panic!(
                                "overflow when calculate the limit of total difficulty, \
                                total: {}, current: {}, index: {}/{}, tau: {}, \
                                state: {}, trend: {:?}, details: {:?}",
                                total, curr, index, epochs_count, tau, state, self, details
                            );
                        })
                    }
                }
            }
        }
        total
    }
}

impl EpochCountGroupByTrend {
    pub(crate) fn subtract1(self) -> Self {
        match self {
            Self::Increased(count) => Self::Increased(count - 1),
            Self::Decreased(count) => Self::Decreased(count - 1),
        }
    }

    pub(crate) fn epochs_count(self) -> u64 {
        match self {
            Self::Increased(count) | Self::Decreased(count) => count,
        }
    }
}

impl EpochDifficultyTrendDetails {
    pub(crate) fn remove_last_epoch(self) -> Self {
        let Self { start, end } = self;
        if end.epochs_count() == 0 {
            Self {
                start: start.subtract1(),
                end,
            }
        } else {
            Self {
                start,
                end: end.subtract1(),
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn total_epochs_count(&self) -> u64 {
        self.start.epochs_count() + self.end.epochs_count()
    }
}

struct TotalDifficulties {
    parent: U256,
    current: U256,
}

impl fmt::Display for TotalDifficulties {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "[{:#x}, {:#x}]", self.parent, self.current)
    }
}

macro_rules! trace_sample {
    ($difficulty:ident, $number:ident, $total_diff:ident, $state:literal, $action:literal) => {
        trace!(
            "difficulty sample {:#x} {} block {} ({}) {}",
            $difficulty,
            $state,
            $number,
            $total_diff,
            $action
        );
    };
}

// Check if the response is matched the last request.
// - Check reorg blocks if there has any.
// - Check the difficulties.
// - Check the difficulty boundary.
pub(crate) fn check_if_response_is_matched(
    last_n_blocks: usize,
    prev_request: &packed::GetLastStateProof,
    headers: &[VerifiableHeader],
    last_header: &VerifiableHeader,
) -> Result<(usize, usize, usize), Status> {
    if headers.is_empty() {
        let errmsg = "headers should NOT be empty";
        return Err(StatusCode::MalformedProtocolMessage.with_context(errmsg));
    }

    // Headers should be sorted.
    if headers
        .windows(2)
        .any(|hs| hs[0].header().number() >= hs[1].header().number())
    {
        let errmsg = "headers should be sorted (monotonic increasing)";
        return Err(StatusCode::MalformedProtocolMessage.with_context(errmsg));
    }

    let total_count = headers.len();

    let start_number: BlockNumber = prev_request.start_number().unpack();
    let reorg_count = headers
        .iter()
        .take_while(|h| h.header().number() < start_number)
        .count();

    if reorg_count != 0 {
        // The count of reorg blocks should be `last_n_blocks`, unless the blocks are not enough.
        if reorg_count != last_n_blocks {
            let first_reorg_header = headers[0].header();
            // Genesis block doesn't have chain root, so blocks should be started from 1.
            if first_reorg_header.number() != 1 {
                let errmsg = format!(
                    "failed to verify reorg last n headers since the count (={}) should be {} \
                    or the number(={}) of the first reorg block (hash: {:#x}) should 1,",
                    reorg_count,
                    last_n_blocks,
                    first_reorg_header.number(),
                    first_reorg_header.hash()
                );
                return Err(StatusCode::InvalidReorgHeaders.with_context(errmsg));
            }
        }
        // The last header in `reorg_last_n_headers` should be continuous.
        let last_reorg_header = headers[reorg_count - 1].header();
        if last_reorg_header.number() != start_number - 1 {
            let errmsg = format!(
                "failed to verify reorg last n headers \
                since they end at block#{} (hash: {:#x}) but we expect block#{}",
                last_reorg_header.number(),
                last_reorg_header.hash(),
                start_number - 1,
            );
            return Err(StatusCode::InvalidReorgHeaders.with_context(errmsg));
        }
    }

    let (sampled_count, last_n_count) = if total_count - reorg_count > last_n_blocks {
        let difficulty_boundary: U256 = prev_request.difficulty_boundary().unpack();
        let before_boundary_count = headers
            .iter()
            .take_while(|h| h.total_difficulty() < difficulty_boundary)
            .count();
        let last_n_count = total_count - before_boundary_count;
        if last_n_count > last_n_blocks {
            (before_boundary_count - reorg_count, last_n_count)
        } else {
            (total_count - reorg_count - last_n_blocks, last_n_blocks)
        }
    } else {
        (0, total_count - reorg_count)
    };

    if sampled_count == 0 {
        if last_n_count > 0 {
            // If no sampled headers, the last_n_blocks should be all new blocks.
            let first_last_n_header_number = headers[reorg_count].header().number();
            let last_last_n_header_number = headers[headers.len() - 1].header().number();
            let last_number = last_header.header().number();
            if first_last_n_header_number != start_number
                || last_last_n_header_number + 1 != last_number
            {
                let errmsg = format!(
                "there should be all blocks of [{}, {}) since no sampled blocks, but got [{}, {}]",
                start_number, last_number, first_last_n_header_number, last_last_n_header_number
            );
                return Err(StatusCode::MalformedProtocolMessage.with_context(errmsg));
            }
        }
    } else {
        // Check if the sampled headers are subject to requested difficulties distribution.
        let first_last_n_total_difficulty: U256 =
            headers[reorg_count + sampled_count].total_difficulty();

        if log_enabled!(Level::Trace) {
            output_debug_messages(prev_request, headers, &first_last_n_total_difficulty);
        }

        let mut difficulties: Vec<U256> = prev_request
            .difficulties()
            .into_iter()
            .map(|item| item.unpack())
            .take_while(|d| d < &first_last_n_total_difficulty)
            .collect();

        for item in &headers[reorg_count..reorg_count + sampled_count] {
            let header = item.header();
            let num = header.number();

            let total_diff = TotalDifficulties {
                parent: item.parent_chain_root().total_difficulty().unpack(),
                current: item.total_difficulty(),
            };

            let mut is_valid = false;
            // Total difficulty for any sampled blocks should be valid.
            while let Some(diff) = difficulties.first().cloned() {
                if is_valid {
                    if diff <= total_diff.current {
                        // Current difficulty has same sample as previous difficulty,
                        // and the sample is current block.
                        trace_sample!(diff, num, total_diff, "in", "skipped");
                        difficulties.remove(0);
                        continue;
                    } else {
                        trace_sample!(diff, num, total_diff, ">>", "unsure");
                        break;
                    }
                } else if total_diff.parent < diff && diff <= total_diff.current {
                    // Current difficulty has one sample, and the sample is current block.
                    trace_sample!(diff, num, total_diff, "in", "found");
                    difficulties.remove(0);
                    is_valid = true;
                } else {
                    trace_sample!(diff, num, total_diff, "??", "invalid");
                    break;
                }
            }

            if !is_valid {
                error!(
                    "failed: block {} (hash: {:#x}) is not a valid sample, \
                    its total-difficulties is {}.",
                    header.number(),
                    header.hash(),
                    total_diff,
                );
                return Err(StatusCode::InvalidSamples.into());
            }
        }

        if !difficulties.is_empty() {
            for diff in &difficulties {
                debug!("difficulty sample {:#x} has no matched blocks", diff);
            }
            let next_difficulty = difficulties
                .first()
                .cloned()
                .expect("checked: difficulties is not empty");
            let last_sampled_header = &headers[reorg_count + sampled_count - 1];
            let first_last_n_header = &headers[reorg_count + sampled_count];
            let previous_total_diff_before_last_n: U256 = first_last_n_header
                .parent_chain_root()
                .total_difficulty()
                .unpack();
            if next_difficulty <= previous_total_diff_before_last_n {
                error!(
                    "failed: there should at least exist one block between \
                    numbers {} and {} (difficulties: {:#x}, ..., {:#x}, {:#x}), \
                    next difficulty sample is {:#x}",
                    last_sampled_header.header().number(),
                    first_last_n_header.header().number(),
                    last_sampled_header.total_difficulty(),
                    previous_total_diff_before_last_n,
                    first_last_n_header.total_difficulty(),
                    next_difficulty
                );
                return Err(StatusCode::InvalidSamples.into());
            }
        }
    }

    Ok((reorg_count, sampled_count, last_n_count))
}

fn output_debug_messages(
    prev_request: &packed::GetLastStateProof,
    headers: &[VerifiableHeader],
    last_n_start: &U256,
) {
    let mut difficulties = prev_request
        .difficulties()
        .into_iter()
        .map(|item| item.unpack())
        .peekable();
    let mut headers = headers.iter().peekable();

    let mut checked_last_n_start = false;
    let mut checked_boundary = false;
    let boundary: U256 = prev_request.difficulty_boundary().unpack();

    loop {
        match (difficulties.peek(), headers.peek()) {
            (Some(ref d), Some(h)) => {
                let total_diff = h.total_difficulty();
                let number = h.header().number();
                if **d <= total_diff {
                    debug!("----- ---------  difficulty {:#x}", d);
                    let _ = difficulties.next();
                } else {
                    if !checked_boundary && boundary <= total_diff {
                        checked_boundary = true;
                        debug!("##### #########  boundary   {:#x}", boundary);
                    }
                    if !checked_last_n_start && *last_n_start <= total_diff {
                        checked_last_n_start = true;
                        debug!("##### #########  last-n     {:#x}", last_n_start);
                    }
                    debug!("block {:9}: difficulty {:#x}", number, total_diff);
                    let _ = headers.next();
                }
                continue;
            }
            (Some(_), None) => {
                for d in difficulties {
                    debug!("----- ---------  difficulty {:#x}", d);
                }
            }
            (None, Some(_)) => {
                for h in headers {
                    let total_diff = h.total_difficulty();
                    let number = h.header().number();
                    if !checked_boundary && boundary <= total_diff {
                        checked_boundary = true;
                        debug!("##### #########  boundary   {:#x}", boundary);
                    }
                    if !checked_last_n_start && *last_n_start <= total_diff {
                        checked_last_n_start = true;
                        debug!("##### #########  last-n     {:#x}", last_n_start);
                    }
                    debug!("block {:9}: difficulty {:#x}", number, total_diff);
                }
            }
            (None, None) => {}
        }
        break;
    }
}

pub(crate) fn verify_tau(
    start_epoch: EpochNumberWithFraction,
    start_compact_target: u32,
    end_epoch: EpochNumberWithFraction,
    end_compact_target: u32,
    tau: u64,
) -> Result<bool, Status> {
    if start_epoch.number() == end_epoch.number() {
        trace!("skip checking TAU since headers in the same epoch",);
        if start_compact_target != end_compact_target {
            error!("failed: different compact targets for a same epoch");
            return Err(StatusCode::InvalidCompactTarget.into());
        }
        Ok(true)
    } else {
        let start_block_difficulty = compact_to_difficulty(start_compact_target);
        let end_block_difficulty = compact_to_difficulty(end_compact_target);
        let start_epoch_difficulty = start_block_difficulty * start_epoch.length();
        let end_epoch_difficulty = end_block_difficulty * end_epoch.length();
        // How many times are epochs switched?
        let epochs_switch_count = end_epoch.number() - start_epoch.number();
        let epoch_difficulty_trend =
            EpochDifficultyTrend::new(&start_epoch_difficulty, &end_epoch_difficulty);
        Ok(epoch_difficulty_trend.check_tau(tau, epochs_switch_count))
    }
}

pub(crate) fn verify_total_difficulty(
    start_epoch: EpochNumberWithFraction,
    start_compact_target: u32,
    start_total_difficulty: &U256,
    end_epoch: EpochNumberWithFraction,
    end_compact_target: u32,
    end_total_difficulty: &U256,
    tau: u64,
) -> Result<(), String> {
    if start_total_difficulty > end_total_difficulty {
        let errmsg = format!(
            "failed since total difficulty is decreased from {:#x} to {:#x} \
            during epochs ([{:#},{:#}])",
            start_total_difficulty, end_total_difficulty, start_epoch, end_epoch
        );
        return Err(errmsg);
    }

    let total_difficulty = end_total_difficulty - start_total_difficulty;
    let start_block_difficulty = &compact_to_difficulty(start_compact_target);

    if start_epoch.number() == end_epoch.number() {
        let total_blocks_count = end_epoch.index() - start_epoch.index();
        let total_difficulty_calculated = start_block_difficulty * total_blocks_count;
        if total_difficulty != total_difficulty_calculated {
            let errmsg = format!(
                "failed since total difficulty is {:#x} \
                but the calculated is {:#x} (= {:#x} * {}) \
                during epochs ([{:#},{:#}])",
                total_difficulty,
                total_difficulty_calculated,
                start_block_difficulty,
                total_blocks_count,
                start_epoch,
                end_epoch
            );
            return Err(errmsg);
        }
    } else {
        let end_block_difficulty = &compact_to_difficulty(end_compact_target);

        let start_epoch_difficulty = start_block_difficulty * start_epoch.length();
        let end_epoch_difficulty = end_block_difficulty * end_epoch.length();
        // How many times are epochs switched?
        let epochs_switch_count = end_epoch.number() - start_epoch.number();
        let epoch_difficulty_trend =
            EpochDifficultyTrend::new(&start_epoch_difficulty, &end_epoch_difficulty);

        // Step-1 Check the magnitude of the difficulty changes.
        let k = epoch_difficulty_trend
            .calculate_tau_exponent(tau, epochs_switch_count)
            .ok_or_else(|| {
                format!(
                    "failed since the epoch difficulty changed \
                    too fast ({:#x}->{:#x}) during epochs ([{:#},{:#}])",
                    start_epoch_difficulty, end_epoch_difficulty, start_epoch, end_epoch
                )
            })?;

        // Step-2 Check the range of total difficulty.
        let start_epoch_blocks_count = start_epoch.length() - start_epoch.index() - 1;
        let end_epoch_blocks_count = end_epoch.index() + 1;
        let unaligned_difficulty_calculated = start_block_difficulty * start_epoch_blocks_count
            + end_block_difficulty * end_epoch_blocks_count;
        if epochs_switch_count == 1 {
            if total_difficulty != unaligned_difficulty_calculated {
                let errmsg = format!(
                    "failed since total difficulty is {:#x} \
                    but the calculated is {:#x} (= {:#x} * {} + {:#x} * {}) \
                    during epochs ([{:#},{:#}])",
                    total_difficulty,
                    unaligned_difficulty_calculated,
                    start_block_difficulty,
                    start_epoch_blocks_count,
                    end_block_difficulty,
                    end_epoch_blocks_count,
                    start_epoch,
                    end_epoch
                );
                return Err(errmsg);
            }
        } else {
            // `k < n` was checked in Step-1.
            // `n / 2 >= 1` was checked since the above branch.
            let n = epochs_switch_count;
            let diff = &start_epoch_difficulty;
            let aligned_difficulty_min = {
                let details = epoch_difficulty_trend
                    .split_epochs(EstimatedLimit::Min, n, k)
                    .remove_last_epoch();
                epoch_difficulty_trend.calculate_total_difficulty_limit(diff, tau, &details)
            };
            let aligned_difficulty_max = {
                let details = epoch_difficulty_trend
                    .split_epochs(EstimatedLimit::Max, n, k)
                    .remove_last_epoch();
                epoch_difficulty_trend.calculate_total_difficulty_limit(diff, tau, &details)
            };
            let total_difficulity_min = &unaligned_difficulty_calculated + &aligned_difficulty_min;
            let total_difficulity_max = &unaligned_difficulty_calculated + &aligned_difficulty_max;
            if total_difficulty < total_difficulity_min || total_difficulty > total_difficulity_max
            {
                let errmsg = format!(
                    "failed since total difficulty ({:#x}) isn't in the range ({:#x}+[{:#x},{:#x}]) \
                    during epochs ([{:#},{:#}])",
                    total_difficulty,
                    unaligned_difficulty_calculated,
                    aligned_difficulty_min,
                    aligned_difficulty_max,
                    start_epoch,
                    end_epoch
                );
                return Err(errmsg);
            }
        }
    }

    Ok(())
}

pub(crate) fn check_continuous_headers(headers: &[HeaderView]) -> Result<(), Status> {
    for pair in headers.windows(2) {
        if !pair[0].is_parent_of(&pair[1]) {
            let errmsg = format!(
                "failed to verify parent for block (number: {}, epoch: {:#}, hash: {:#x}, parent: {:#x}), \
                because parent block is (number: {}, epoch: {:#}, hash: {:#x})",
                pair[1].number(),
                pair[1].epoch(),
                pair[1].hash(),
                pair[1].parent_hash(),
                pair[0].number(),
                pair[0].epoch(),
                pair[0].hash(),
            );
            return Err(StatusCode::InvalidParentBlock.with_context(errmsg));
        }
    }
    Ok(())
}

pub(crate) fn verify_mmr_proof<'a, T: Iterator<Item = &'a HeaderView>>(
    mmr_activated_epoch: EpochNumber,
    last_header: &VerifiableHeader,
    raw_proof: packed::HeaderDigestVecReader,
    headers: T,
) -> Result<(), Status> {
    if last_header.is_valid(mmr_activated_epoch) {
        trace!(
            "passed: verify extra hash for block-{} ({:#x})",
            last_header.header().number(),
            last_header.header().hash(),
        );
    } else {
        let errmsg = format!(
            "failed to verify extra hash for block-{} ({:#x})",
            last_header.header().number(),
            last_header.header().hash(),
        );
        return Err(StatusCode::InvalidProof.with_context(errmsg));
    };
    let parent_chain_root = last_header.parent_chain_root();
    let proof: MMRProof = {
        let mmr_size = leaf_index_to_mmr_size(parent_chain_root.end_number().unpack());
        let proof = raw_proof
            .iter()
            .map(|header_digest| header_digest.to_entity())
            .collect();
        MMRProof::new(mmr_size, proof)
    };

    let digests_with_positions = {
        let res = headers
            .map(|header| {
                let index = header.number();
                let position = leaf_index_to_pos(index);
                let digest = header.digest();
                digest.verify()?;
                Ok((position, digest))
            })
            .collect::<Result<Vec<_>, String>>();
        match res {
            Ok(tmp) => tmp,
            Err(err) => {
                let errmsg = format!("failed to verify all digest since {}", err);
                return Err(StatusCode::InvalidProof.with_context(errmsg));
            }
        }
    };
    let verify_result = match proof.verify(parent_chain_root, digests_with_positions) {
        Ok(verify_result) => verify_result,
        Err(err) => {
            let errmsg = format!("failed to verify the proof since {}", err);
            return Err(StatusCode::InvalidProof.with_context(errmsg));
        }
    };
    if verify_result {
        trace!("passed: verify mmr proof");
    } else {
        let errmsg = "failed to verify the mmr proof since the result is false";
        return Err(StatusCode::InvalidProof.with_context(errmsg));
    }
    Ok(())
}
