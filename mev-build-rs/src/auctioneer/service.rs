use crate::{
    auctioneer::auction_schedule::{AuctionSchedule, Proposals, Proposer, RelaySet},
    bidder::Message as BidderMessage,
    payload::attributes::{BuilderPayloadBuilderAttributes, ProposalAttributes},
    service::ClockMessage,
    utils::compat::{to_blobs_bundle, to_bytes20, to_bytes32, to_execution_payload},
    Error,
};
use ethereum_consensus::{
    clock::convert_timestamp_to_slot,
    crypto::SecretKey,
    primitives::{BlsPublicKey, Epoch, Slot},
    state_transition::Context,
    Fork,
};
use mev_rs::{
    relay::parse_relay_endpoints,
    signing::sign_builder_message,
    types::{block_submission, BidTrace, SignedBidSubmission},
    BlindedBlockRelayer, Relay,
};
use reth::{
    api::{EngineTypes, PayloadBuilderAttributes},
    payload::{EthBuiltPayload, Events, PayloadBuilderHandle, PayloadId, PayloadStore},
};
use serde::Deserialize;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use tokio::sync::{
    broadcast,
    mpsc::{Receiver, Sender},
};
use tokio_stream::StreamExt;
use tracing::{debug, error, info, trace, warn};

// Fetch new proposer schedules from all connected relays at this period into the epoch
// E.g. a value of `2` corresponds to being half-way into the epoch.
const PROPOSAL_SCHEDULE_INTERVAL: u64 = 2;

fn make_attributes_for_proposer(
    attributes: &BuilderPayloadBuilderAttributes,
    proposer: &Proposer,
) -> BuilderPayloadBuilderAttributes {
    let proposal = ProposalAttributes {
        proposer_gas_limit: proposer.gas_limit,
        proposer_fee_recipient: proposer.fee_recipient,
    };
    let mut attributes = attributes.clone();
    attributes.attach_proposal(proposal);
    attributes
}

fn prepare_submission(
    payload: EthBuiltPayload,
    signing_key: &SecretKey,
    public_key: &BlsPublicKey,
    auction_context: &AuctionContext,
    context: &Context,
) -> Result<SignedBidSubmission, Error> {
    let message = BidTrace {
        slot: auction_context.slot,
        parent_hash: to_bytes32(auction_context.attributes.inner.parent),
        block_hash: to_bytes32(payload.block().hash()),
        builder_public_key: public_key.clone(),
        proposer_public_key: auction_context.proposer.public_key.clone(),
        proposer_fee_recipient: to_bytes20(auction_context.proposer.fee_recipient),
        gas_limit: payload.block().gas_limit,
        gas_used: payload.block().gas_used,
        value: payload.fees(),
    };
    let execution_payload = to_execution_payload(payload.block());
    let signature = sign_builder_message(&message, signing_key, context)?;
    let submission = match execution_payload.version() {
        Fork::Bellatrix => {
            SignedBidSubmission::Bellatrix(block_submission::bellatrix::SignedBidSubmission {
                message,
                execution_payload,
                signature,
            })
        }
        Fork::Capella => {
            SignedBidSubmission::Capella(block_submission::capella::SignedBidSubmission {
                message,
                execution_payload,
                signature,
            })
        }
        Fork::Deneb => SignedBidSubmission::Deneb(block_submission::deneb::SignedBidSubmission {
            message,
            execution_payload,
            blobs_bundle: to_blobs_bundle(payload.sidecars()),
            signature,
        }),
        other => unreachable!("fork {other} is not reachable from this type"),
    };
    Ok(submission)
}

#[derive(Debug)]
pub struct AuctionContext {
    pub slot: Slot,
    pub attributes: BuilderPayloadBuilderAttributes,
    pub proposer: Proposer,
    pub relays: RelaySet,
}

#[derive(Deserialize, Debug, Default, Clone)]
pub struct Config {
    /// Secret key used to sign builder messages to relay
    pub secret_key: SecretKey,
    #[serde(skip)]
    /// Public key corresponding to secret key
    pub public_key: BlsPublicKey,
    /// List of relays to submit bids
    pub relays: Vec<String>,
}

pub struct Service<
    Engine: EngineTypes<
        PayloadBuilderAttributes = BuilderPayloadBuilderAttributes,
        BuiltPayload = EthBuiltPayload,
    >,
> {
    clock: broadcast::Receiver<ClockMessage>,
    builder: PayloadBuilderHandle<Engine>,
    payload_store: PayloadStore<Engine>,
    relays: Vec<Relay>,
    config: Config,
    context: Arc<Context>,
    // TODO consolidate this somewhere...
    genesis_time: u64,
    bidder_tx: Sender<BidderMessage>,
    bidder_rx: Receiver<BidderMessage>,

    auction_schedule: AuctionSchedule,
    open_auctions: HashMap<PayloadId, Arc<AuctionContext>>,
    processed_payload_attributes: HashMap<Slot, HashSet<PayloadId>>,
}

impl<
        Engine: EngineTypes<
                PayloadBuilderAttributes = BuilderPayloadBuilderAttributes,
                BuiltPayload = EthBuiltPayload,
            > + 'static,
    > Service<Engine>
{
    pub fn new(
        clock: broadcast::Receiver<ClockMessage>,
        builder: PayloadBuilderHandle<Engine>,
        bidder_tx: Sender<BidderMessage>,
        bidder_rx: Receiver<BidderMessage>,
        mut config: Config,
        context: Arc<Context>,
        genesis_time: u64,
    ) -> Self {
        let relays =
            parse_relay_endpoints(&config.relays).into_iter().map(Relay::from).collect::<Vec<_>>();

        config.public_key = config.secret_key.public_key();

        let payload_store = builder.clone().into();

        Self {
            clock,
            builder,
            payload_store,
            relays,
            config,
            context,
            genesis_time,
            bidder_tx,
            bidder_rx,
            auction_schedule: Default::default(),
            open_auctions: Default::default(),
            processed_payload_attributes: Default::default(),
        }
    }

    async fn fetch_proposer_schedules(&mut self) {
        // TODO: consider moving to new task on another thread, can do parallel fetch (join set)
        // and not block others at this interval
        // TODO: batch updates to auction schedule
        // TODO: consider fast data access once this stabilizes
        // TODO: rework `auction_schedule` so there is no issue with confusing relays and their
        // indices
        for (relay_index, relay) in self.relays.iter().enumerate() {
            match relay.get_proposal_schedule().await {
                Ok(schedule) => {
                    let slots = self.auction_schedule.process(relay_index, &schedule);
                    info!(?slots, %relay, "processed proposer schedule");
                }
                Err(err) => {
                    warn!(err = %err, "error fetching proposer schedule from relay")
                }
            }
        }
    }

    async fn on_slot(&mut self, slot: Slot) {
        debug!(slot, "processed");
        if (slot * PROPOSAL_SCHEDULE_INTERVAL) % self.context.slots_per_epoch == 0 {
            self.fetch_proposer_schedules().await;
        }
    }

    async fn on_epoch(&mut self, epoch: Epoch) {
        debug!(epoch, "processed");
        // NOTE: clear stale state
        let retain_slot = epoch * self.context.slots_per_epoch;
        self.auction_schedule.clear(retain_slot);
        self.open_auctions.retain(|_, auction| auction.slot >= retain_slot);
        self.processed_payload_attributes.retain(|&slot, _| slot >= retain_slot);
    }

    fn get_proposals(&self, slot: Slot) -> Option<&Proposals> {
        self.auction_schedule.get_matching_proposals(slot)
    }

    async fn process_proposals(
        &self,
        slot: Slot,
        attributes: BuilderPayloadBuilderAttributes,
        proposals: &Proposals,
    ) -> Vec<AuctionContext> {
        let mut new_auctions = vec![];
        for (proposer, relays) in proposals {
            let attributes = make_attributes_for_proposer(&attributes, proposer);

            if self.start_build(&attributes).await.is_some() {
                // TODO: can likely skip full attributes in `AuctionContext`
                // TODO: consider data layout here...
                // TODO: can likely refactor around auction schedule to skip some clones...
                let auction = AuctionContext {
                    slot,
                    attributes,
                    proposer: proposer.clone(),
                    relays: relays.clone(),
                };
                new_auctions.push(auction);
            }
        }
        new_auctions
    }

    async fn start_build(&self, attributes: &BuilderPayloadBuilderAttributes) -> Option<PayloadId> {
        // TODO: necessary to get response, other than no error?
        match self.builder.new_payload(attributes.clone()).await {
            Ok(payload_id) => {
                let attributes_payload_id = attributes.payload_id();
                if payload_id != attributes_payload_id {
                    error!(%payload_id, %attributes_payload_id, "mismatch between computed payload id and the one returned by the payload builder");
                }
                Some(payload_id)
            }
            Err(err) => {
                warn!(%err, "builder could not start build with payload builder");
                None
            }
        }
    }

    async fn process_new_auction(&mut self, auction: AuctionContext) {
        let payload_id = auction.attributes.payload_id();
        // TODO: consider data layout in `open_auctions`
        let auction = self.open_auctions.entry(payload_id).or_insert_with(|| Arc::new(auction));

        self.bidder_tx.send(BidderMessage::NewAuction(auction.clone())).await.expect("can send");
    }

    async fn on_payload_attributes(&mut self, attributes: BuilderPayloadBuilderAttributes) {
        let slot = convert_timestamp_to_slot(
            attributes.timestamp(),
            self.genesis_time,
            self.context.seconds_per_slot,
        )
        .expect("is past genesis");

        let processed_set = self.processed_payload_attributes.entry(slot).or_default();
        let is_new = processed_set.insert(attributes.payload_id());

        if !is_new {
            trace!(payload_id = %attributes.payload_id(), "ignoring duplicate payload attributes");
            return
        }

        // TODO: consolidate once stable
        if let Some(proposals) = self.get_proposals(slot) {
            let auctions = self.process_proposals(slot, attributes, proposals).await;
            for auction in auctions {
                self.process_new_auction(auction).await;
            }
        }
    }

    async fn process_bid_update(&mut self, message: BidderMessage) {
        match message {
            BidderMessage::RevenueQuery(payload_id, tx) => {
                // TODO: store this payload (by hash) so that the bid that returns targets something
                // stable...
                if let Some(payload) = self.payload_store.best_payload(payload_id).await {
                    match payload {
                        Ok(payload) => {
                            // TODO: send more dynamic updates
                            // by the time the bidder submits a value the best payload may have
                            // already changed
                            tx.send(Some(payload.fees())).expect("can send");
                            return
                        }
                        Err(err) => warn!(%err, "could not get best payload from payload store"),
                    }
                }
                // NOTE: if no payload was found, the auction has been terminated
                if let Err(err) = tx.send(None) {
                    warn!(?err, "could not send after failure to retrieve payload");
                }
            }
            BidderMessage::Dispatch { payload_id, value: _value, keep_alive: _keep_alive } => {
                // TODO: forward keep alive signal to builder
                // TODO: sort out streaming comms to builder
                // TOOD: backpressure on bidder...?
                if let Some(payload) = self.payload_store.resolve(payload_id).await {
                    match payload {
                        Ok(payload) => self.submit_payload(payload).await,
                        Err(err) => warn!(%err, "payload resolution failed"),
                    }
                }
            }
            _ => {}
        }
    }

    async fn submit_payload(&self, payload: EthBuiltPayload) {
        let auction = self.open_auctions.get(&payload.id()).expect("has auction");
        let relay_set = auction
            .relays
            .iter()
            .map(|&index| format!("{0}", self.relays[index]))
            .collect::<Vec<_>>();
        info!(
            slot = auction.slot,
            block_number = payload.block().number,
            block_hash = %payload.block().hash(),
            parent_hash = %payload.block().header.header().parent_hash,
            txn_count = %payload.block().body.len(),
            blob_count = %payload.sidecars().iter().map(|s| s.blobs.len()).sum::<usize>(),
            value = %payload.fees(),
            relays=?relay_set,
            "submitting payload"
        );
        match prepare_submission(
            payload,
            &self.config.secret_key,
            &self.config.public_key,
            auction,
            &self.context,
        ) {
            Ok(signed_submission) => {
                // TODO: parallel dispatch
                for &relay_index in &auction.relays {
                    match self.relays.get(relay_index) {
                        Some(relay) => {
                            if let Err(err) = relay.submit_bid(&signed_submission).await {
                                warn!(%err, ?relay, slot = auction.slot, "could not submit payload");
                            }
                        }
                        None => {
                            // NOTE: this arm signals a violation of an internal invariant
                            // Please fix if you see this error
                            error!(relay_index, "could not dispatch to unknown relay");
                        }
                    }
                }
            }
            Err(err) => {
                warn!(%err, slot = auction.slot, "could not prepare submission")
            }
        }
    }

    async fn process_clock(&mut self, message: ClockMessage) {
        use ClockMessage::*;
        match message {
            NewSlot(slot) => self.on_slot(slot).await,
            NewEpoch(epoch) => self.on_epoch(epoch).await,
        }
    }

    async fn process_payload_event(&mut self, event: Events<Engine>) {
        if let Events::Attributes(attributes) = event {
            self.on_payload_attributes(attributes).await;
        }
    }

    pub async fn spawn(mut self) {
        if self.relays.is_empty() {
            warn!("no valid relays provided in config");
        } else {
            let count = self.relays.len();
            info!(count, relays = ?self.relays, "configured with relay(s)");
        }

        // initialize proposer schedule
        self.fetch_proposer_schedules().await;

        let mut payload_events =
            self.builder.subscribe().await.expect("can subscribe to events").into_stream();

        loop {
            tokio::select! {
                Ok(message) = self.clock.recv() => self.process_clock(message).await,
                Some(event) = payload_events.next() => match event {
                    Ok(event) =>  self.process_payload_event(event).await,
                    Err(err) => warn!(%err, "error getting payload event"),
                },
                Some(message) = self.bidder_rx.recv() => self.process_bid_update(message).await,
            }
        }
    }
}
