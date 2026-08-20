#![no_main]

use excise::model::{ByteBounds, IdentityStore, NodeId};
use file_id::FileId;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut store = IdentityStore::new(64 * 1024);
    let mut total = ByteBounds::default();
    for (index, chunk) in data.get(1..).unwrap_or_default().chunks(3).take(64).enumerate() {
        let lower = u128::from(chunk.first().copied().unwrap_or_default());
        let upper = chunk.get(1).copied().and_then(|value| {
            (value & 1 == 0).then_some(lower.saturating_add(u128::from(value)))
        });
        let bounds = ByteBounds { lower, upper };
        total.add(bounds);
        let file_id = FileId::new_inode(u64::from(chunk.get(2).copied().unwrap_or_default()), index as u64);
        let _ = store.observe(
            &file_id,
            Some(1),
            bounds,
            Some(NodeId(u32::try_from(index).unwrap_or(u32::MAX))),
            Some(NodeId(u32::try_from(index).unwrap_or(u32::MAX))),
        );
    }
    let before = total.lower;
    total.subtract(ByteBounds::exact(u128::from(data.first().copied().unwrap_or_default())));
    assert!(total.lower <= before);
    assert!(store.len() <= 64);
});
