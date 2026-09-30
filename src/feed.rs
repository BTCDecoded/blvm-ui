//! Live chain feed: newest items (enough to fill a wide block rail). Fast IBD is chunked; slow tip follow is one height at a time.

use serde::Serialize;
use std::collections::VecDeque;

pub const FEED_CAP: usize = 24;
/// If height jumps by this many or more in one sample, collapse into one chunk.
pub const CHUNK_MIN: u64 = 8;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FeedItem {
    pub kind: &'static str,
    pub start: u64,
    pub end: u64,
    pub label: String,
    pub key: String,
}

impl FeedItem {
    pub fn block(height: u64) -> Self {
        Self {
            kind: "block",
            start: height,
            end: height,
            label: height.to_string(),
            key: format!("block:{height}"),
        }
    }

    pub fn chunk(start: u64, end: u64) -> Self {
        let (start, end) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        Self {
            kind: "chunk",
            start,
            end,
            label: if start == end {
                start.to_string()
            } else {
                format!("{start}–{end}")
            },
            key: format!("chunk:{start}:{end}"),
        }
    }
}

/// Newest-first feed. `last` is the last height already represented.
pub fn advance_feed(feed: &mut VecDeque<FeedItem>, last: &mut Option<u64>, new_height: u64) -> u64 {
    let Some(prev) = *last else {
        *last = Some(new_height);
        feed.push_front(FeedItem::block(new_height));
        trim(feed);
        return 0;
    };
    if new_height == prev {
        return 0;
    }
    // Height went backwards: datadir wipe, reindex, or reorg. Drop tiles that
    // no longer exist on this chain. Network switch is a different feed map.
    if new_height < prev {
        feed.retain(|item| item.end <= new_height);
        *last = Some(new_height);
        if feed.is_empty() {
            feed.push_front(FeedItem::block(new_height));
        }
        trim(feed);
        return 0;
    }
    let delta = new_height - prev;
    if delta >= CHUNK_MIN {
        feed.push_front(FeedItem::chunk(prev.saturating_add(1), new_height));
    } else {
        for h in (prev + 1)..=new_height {
            feed.push_front(FeedItem::block(h));
        }
    }
    *last = Some(new_height);
    trim(feed);
    delta
}

fn trim(feed: &mut VecDeque<FeedItem>) {
    while feed.len() > FEED_CAP {
        feed.pop_back();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_sample_is_one_block() {
        let mut feed = VecDeque::new();
        let mut last = None;
        advance_feed(&mut feed, &mut last, 100);
        assert_eq!(feed.len(), 1);
        assert_eq!(feed[0], FeedItem::block(100));
    }

    #[test]
    fn slow_tip_appends_each_height() {
        let mut feed = VecDeque::new();
        let mut last = None;
        advance_feed(&mut feed, &mut last, 10);
        advance_feed(&mut feed, &mut last, 11);
        advance_feed(&mut feed, &mut last, 12);
        let labels: Vec<_> = feed.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, ["12", "11", "10"]);
    }

    #[test]
    fn fast_ibd_becomes_a_chunk() {
        let mut feed = VecDeque::new();
        let mut last = None;
        advance_feed(&mut feed, &mut last, 1000);
        advance_feed(&mut feed, &mut last, 5000);
        assert_eq!(feed[0], FeedItem::chunk(1001, 5000));
        assert_eq!(feed.len(), 2);
    }

    #[test]
    fn keeps_only_cap_newest() {
        let mut feed = VecDeque::new();
        let mut last = None;
        advance_feed(&mut feed, &mut last, 1);
        for h in 2..=40 {
            advance_feed(&mut feed, &mut last, h);
        }
        assert_eq!(feed.len(), FEED_CAP);
        assert_eq!(feed.front().unwrap().label, "40");
        assert_eq!(feed.back().unwrap().label, (40 - FEED_CAP + 1).to_string());
    }

    #[test]
    fn chunk_normalizes_reversed_range() {
        assert_eq!(FeedItem::chunk(5000, 1001).label, "1001–5000");
    }

    #[test]
    fn wipe_to_genesis_drops_old_tiles() {
        let mut feed = VecDeque::new();
        let mut last = None;
        advance_feed(&mut feed, &mut last, 152_885);
        advance_feed(&mut feed, &mut last, 0);
        assert_eq!(last, Some(0));
        assert_eq!(feed.len(), 1);
        assert_eq!(feed[0], FeedItem::block(0));
    }

    #[test]
    fn small_reorg_drops_only_invalid_heights() {
        let mut feed = VecDeque::new();
        let mut last = None;
        advance_feed(&mut feed, &mut last, 10);
        advance_feed(&mut feed, &mut last, 11);
        advance_feed(&mut feed, &mut last, 12);
        advance_feed(&mut feed, &mut last, 11);
        let labels: Vec<_> = feed.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, ["11", "10"]);
        assert_eq!(last, Some(11));
    }
}
