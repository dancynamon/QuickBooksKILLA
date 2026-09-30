//! Walmart and Etsy: parser trait and nothing else. No fake logic.
//!
//! Both channels are blocked on data, not on code: Walmart is pending a
//! Seller Center login for the reconciliation report, and Etsy is pending
//! the payment-account CSV links (sent to dan@embrace.yoga). Until a real
//! export exists to read the column names from, guessing a format here would
//! only produce postings that look right and are not.

use super::{Channel, ChannelError, ChannelPosting};

/// What a channel's parser hands back: postings ready for [`super::commit`].
pub trait ChannelParser {
    fn channel(&self) -> Channel;

    /// Parses one export into postings. Must assert clearing nets to the
    /// payout before returning a posting, like the Shopify and Amazon ones.
    fn parse(&self, text: &str) -> Result<Vec<ChannelPosting>, ChannelError>;
}

pub struct WalmartReconParser;

impl ChannelParser for WalmartReconParser {
    fn channel(&self) -> Channel {
        Channel::Walmart
    }

    fn parse(&self, _text: &str) -> Result<Vec<ChannelPosting>, ChannelError> {
        // TODO(D26): Walmart Seller Center reconciliation report, one
        // posting per recon period, clearing 1172, fees 6613, adjustments
        // 6620. Needs a real export to read the columns from.
        Err(ChannelError::NotImplemented(
            "walmart-recon: no export format yet (Seller Center login pending)".into(),
        ))
    }
}

pub struct EtsyParser;

impl ChannelParser for EtsyParser {
    fn channel(&self) -> Channel {
        Channel::Etsy
    }

    fn parse(&self, _text: &str) -> Result<Vec<ChannelPosting>, ChannelError> {
        // TODO(D26): Etsy payment account CSV, one posting per deposit,
        // clearing 1173, fees 6614. Needs a real export to read the columns
        // from.
        Err(ChannelError::NotImplemented(
            "etsy: no export format yet (payment-account CSV links pending)".into(),
        ))
    }
}
