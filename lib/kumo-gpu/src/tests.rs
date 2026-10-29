//j647
//j650
//j651
//j652
//j681

extern crate std;

use super::*;

fn binding() -> Binding {
    Binding {
        vm: 1,
        buffer: 2,
        va: VA_START,
        offset: 0,
        bytes: PAGE_BYTES,
        flags: BindFlags(BindFlags::READ_ONLY | BindFlags::NO_EXECUTE),
    }
}

fn submission() -> Submission {
    Submission {
        group: 3,
        stream_va: VA_START,
        stream_bytes: 48,
        signal_point: 1,
    }
}

#[test]
fn all_requests_round_trip_and_have_a_fixed_versioned_frame() {
    for command in [
        Command::GetInfo,
        Command::GetTopology,
        Command::GetMmuFeatures,
        Command::GetTilerFeatures,
        Command::GetTextureFeatures,
        Command::GetCoreFeatures,
        Command::GetThreadProperties,
        Command::VmCreate,
        Command::BufferCreate { bytes: PAGE_BYTES },
        Command::VmBind(binding()),
        Command::GroupCreate { vm: 1 },
        Command::Submit(submission()),
        Command::SyncRead { group: 3, point: 1 },
        Command::FenceWait { group: 3, point: 1 },
    ] {
        let request = Request {
            id: 0x1234_5678,
            command,
        };
        let wire = request.encode();
        assert_eq!(&wire[..8], b"KGQ1\x78\x56\x34\x12");
        assert_eq!(Request::decode(&wire), Some(request));
        for len in 0..REQUEST_LEN {
            assert_eq!(Request::decode(&wire[..len]), None);
        }
        let mut extra = wire.to_vec();
        extra.push(0);
        assert_eq!(Request::decode(&extra), None);
    }
}

#[test]
fn bind_wire_has_no_native_padding_pointers_or_handles() {
    let mut b = binding();
    b.offset = PAGE_BYTES;
    b.flags = BindFlags(5);
    let wire = Request {
        id: 1,
        command: Command::VmBind(b),
    }
    .encode();
    assert_eq!(&wire[8..16], &[3, 0, 0, 0, 0, 0, 0, 0]);
    let expected: std::vec::Vec<u8> = [1u64, 2, 0x2000_0000, 4096, 4096, 5]
        .into_iter()
        .flat_map(u64::to_le_bytes)
        .collect();
    assert_eq!(&wire[16..], expected);
}

#[test]
fn info_query_has_exact_frames_and_rejects_reserved_or_truncated_capabilities() {
    let request = Request {
        id: 9,
        command: Command::GetInfo,
    }
    .encode();
    assert_eq!(&request[8..16], &[7, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(&request[16..], &[0u8; 48]);
    let mut bad = request;
    bad[63] = 1;
    assert_eq!(Request::decode(&bad), None);
    let reply = Reply {
        id: 9,
        result: Ok(Response::Info(DeviceInfo {
            gpu_id: 0xa8670005,
            cs_features: 0x7085f,
        })),
    };
    let bytes = reply.encode();
    assert_eq!(&bytes[12..16], &[7, 0, 0, 0]);
    assert_eq!(&bytes[16..24], &[5, 0, 0x67, 0xa8, 0, 0, 0, 0]);
    assert_eq!(&bytes[24..32], &[0x5f, 8, 7, 0, 0, 0, 0, 0]);
    assert_eq!(Reply::decode(&bytes), Some(reply));
    assert_eq!(reply.result.unwrap().handle_count(), 0);
    for at in [20, 28, 32, 47] {
        let mut bad = bytes;
        bad[at] = 1;
        assert_eq!(Reply::decode(&bad), None, "byte {at}");
    }
}

#[test]
fn request_decoder_rejects_reserved_bytes_unknown_ops_versions_and_truncation() {
    let request = Request {
        id: 1,
        command: Command::VmCreate,
    };
    let good = request.encode();
    for at in [3, 4, 8, 12, 15, 16, 63] {
        let mut wire = good;
        wire[at] = if at == 4 { 0 } else { 255 };
        assert_eq!(Request::decode(&wire), None, "byte {at}");
    }
    for command in [
        Command::GroupCreate { vm: 1 },
        Command::VmBind(binding()),
        Command::Submit(submission()),
    ] {
        let mut wire = Request { id: 1, command }.encode();
        wire[20] = 1; // High bits of a u32 object ID carried in a u64 slot.
        assert_eq!(Request::decode(&wire), None);
    }
    let mut wire = Request {
        id: 1,
        command: Command::Submit(submission()),
    }
    .encode();
    wire[36] = 1; // High bits of stream_bytes must not narrow to the original length.
    assert_eq!(Request::decode(&wire), None);
}

#[test]
fn malformed_semantic_requests_never_decode() {
    let b = binding();
    let s = submission();
    for command in [
        Command::BufferCreate { bytes: 0 },
        Command::BufferCreate {
            bytes: PAGE_BYTES + 1,
        },
        Command::BufferCreate {
            bytes: MAX_BUFFER_BYTES + PAGE_BYTES,
        },
        Command::VmBind(Binding {
            flags: BindFlags(8),
            ..b
        }),
        Command::VmBind(Binding {
            flags: BindFlags(0),
            ..b
        }),
        Command::VmBind(Binding { vm: 0, ..b }),
        Command::VmBind(Binding { va: VA_END, ..b }),
        Command::VmBind(Binding {
            offset: u64::MAX - 4095,
            ..b
        }),
        Command::GroupCreate { vm: 0 },
        Command::Submit(Submission {
            stream_bytes: 0,
            ..s
        }),
        Command::Submit(Submission {
            stream_va: VA_START + 8,
            ..s
        }),
        Command::Submit(Submission {
            signal_point: 0,
            ..s
        }),
        Command::SyncRead { group: 0, point: 1 },
        Command::SyncRead { group: 3, point: 0 },
    ] {
        assert_eq!(
            Request::decode(&Request { id: 1, command }.encode()),
            None,
            "{command:?}"
        );
    }
}

#[test]
fn replies_distinguish_acceptance_pending_success_and_gpu_error() {
    let responses = [
        Response::Info(DeviceInfo {
            gpu_id: 0xa8670005,
            cs_features: 0x7085f,
        }),
        Response::VmCreated { vm: 1 },
        Response::BufferCreated {
            buffer: 2,
            bytes: PAGE_BYTES,
        },
        Response::VmBound,
        Response::GroupCreated { group: 3 },
        Response::Submitted { point: 1 },
        Response::Sync {
            point: 1,
            state: SyncState::Pending,
        },
        Response::Sync {
            point: 1,
            state: SyncState::Complete { gpu_status: 0 },
        },
        Response::Sync {
            point: 1,
            state: SyncState::Complete { gpu_status: 0x58 },
        },
        Response::Texture(DeviceTexture {
            texture_features: [0x0200_0001, 0, 0x8000_0000, 0xffff_ffff],
        }),
    ];
    for response in responses {
        let reply = Reply {
            id: 7,
            result: Ok(response),
        };
        assert_eq!(Reply::decode(&reply.encode()), Some(reply));
        assert_eq!(
            response.handle_count(),
            usize::from(matches!(response, Response::BufferCreated { .. }))
        );
    }
    for status in [
        Status::BadRequest,
        Status::BadState,
        Status::NotFound,
        Status::Busy,
        Status::NoMemory,
        Status::DeviceLost,
        Status::NotSupported,
    ] {
        let reply = Reply {
            id: 7,
            result: Err(status),
        };
        assert_eq!(Reply::decode(&reply.encode()), Some(reply));
    }
    let bad = Reply {
        id: 0,
        result: Err(Status::BadRequest),
    };
    assert_eq!(Reply::decode(&bad.encode()), Some(bad));
}

#[test]
fn reply_decoder_rejects_noncanonical_or_truncated_frames() {
    let reply = Reply {
        id: 1,
        result: Ok(Response::VmCreated { vm: 2 }),
    };
    let good = reply.encode();
    for len in 0..REPLY_LEN {
        assert_eq!(Reply::decode(&good[..len]), None);
    }
    let mut extra = good.to_vec();
    extra.push(0);
    assert_eq!(Reply::decode(&extra), None);
    for at in [3, 4, 8, 12, 20, 24, 47] {
        let mut wire = good;
        wire[at] = if at == 4 { 0 } else { 255 };
        assert_eq!(Reply::decode(&wire), None, "byte {at}");
    }
    for reply in [
        Reply {
            id: 0,
            result: Err(Status::Busy),
        },
        Reply {
            id: 0,
            result: Ok(Response::VmBound),
        },
        Reply {
            id: 1,
            result: Ok(Response::BufferCreated {
                buffer: 1,
                bytes: 1,
            }),
        },
    ] {
        assert_eq!(Reply::decode(&reply.encode()), None);
    }
    let mut pending = Reply {
        id: 1,
        result: Ok(Response::Sync {
            point: 1,
            state: SyncState::Pending,
        }),
    }
    .encode();
    pending[32] = 1;
    assert_eq!(Reply::decode(&pending), None);
    let mut error = Reply {
        id: 1,
        result: Err(Status::Busy),
    }
    .encode();
    error[16] = 1;
    assert_eq!(Reply::decode(&error), None);
}

#[test]
fn bindings_check_both_extents_overflow_flags_and_overlap() {
    let b = binding();
    assert_eq!(b.validate(PAGE_BYTES), Ok(()));
    assert_eq!(
        Binding {
            offset: PAGE_BYTES,
            ..b
        }
        .validate(PAGE_BYTES * 2),
        Ok(())
    );
    for invalid in [
        Binding {
            va: VA_START - PAGE_BYTES,
            ..b
        },
        Binding { va: VA_END, ..b },
        Binding {
            va: VA_START + 1,
            ..b
        },
        Binding {
            va: u64::MAX - 4095,
            ..b
        },
        Binding {
            offset: PAGE_BYTES,
            ..b
        },
        Binding {
            offset: u64::MAX - 4095,
            ..b
        },
        Binding { bytes: 0, ..b },
        Binding { buffer: 0, ..b },
        Binding { vm: 0, ..b },
        Binding {
            flags: BindFlags(0),
            ..b
        },
        Binding {
            flags: BindFlags(8),
            ..b
        },
    ] {
        assert_eq!(
            invalid.validate(PAGE_BYTES),
            Err(Status::BadRequest),
            "{invalid:?}"
        );
    }
    assert!(b.overlaps(b));
    assert!(!b.overlaps(Binding {
        va: b.va + b.bytes,
        ..b
    }));
    assert!(!b.overlaps(Binding { vm: 2, ..b }));
    assert!(Binding {
        bytes: 2 * PAGE_BYTES,
        ..b
    }
    .overlaps(Binding {
        va: b.va + PAGE_BYTES,
        ..b
    }));
    assert_eq!(
        Binding {
            flags: BindFlags(BindFlags::NO_EXECUTE),
            ..b
        }
        .validate(PAGE_BYTES),
        Ok(())
    );
}

#[test]
fn submit_must_fit_read_only_binding_in_the_groups_vm() {
    let b = binding();
    let s = submission();
    assert_eq!(s.validate_for(1, b), Ok(()));
    assert_eq!(s.validate_for(2, b), Err(Status::BadRequest));
    assert_eq!(
        s.validate_for(
            1,
            Binding {
                flags: BindFlags(BindFlags::NO_EXECUTE),
                ..b
            }
        ),
        Err(Status::BadRequest)
    );
    for s in [
        Submission {
            stream_va: VA_START + PAGE_BYTES,
            ..s
        },
        Submission {
            stream_bytes: PAGE_BYTES as u32 + 8,
            ..s
        },
        Submission {
            stream_bytes: MAX_STREAM_BYTES + 8,
            ..s
        },
        Submission {
            stream_va: u64::MAX - 63,
            ..s
        },
        Submission { group: 0, ..s },
        Submission {
            stream_bytes: 7,
            ..s
        },
    ] {
        assert_eq!(s.validate_for(1, b), Err(Status::BadRequest));
    }
}

#[test]
fn acceptance_is_pending_until_matching_hardware_completion() {
    let mut timeline = Timeline::default();
    assert_eq!(timeline.query(1), Err(Status::NotFound));
    timeline.reserve(1).unwrap();
    assert_eq!(timeline.query(1), Ok(SyncState::Pending));
    let reserved = timeline;
    assert_eq!(timeline.reserve(2), Err(Status::Busy));
    assert_eq!(timeline, reserved);
    assert_eq!(timeline.complete(2, 0), Err(Status::BadState));
    assert_eq!(timeline, reserved);
    timeline.complete(1, 0).unwrap();
    assert_eq!(timeline.query(1), Ok(SyncState::Complete { gpu_status: 0 }));
    assert_eq!(timeline.complete(1, 0), Err(Status::BadState));
    assert_eq!(timeline.reserve(1), Err(Status::BadRequest));
    timeline.reserve(2).unwrap();
    timeline.complete(2, 0).unwrap();
    assert_eq!(timeline.query(1), Ok(SyncState::Complete { gpu_status: 0 }));
    assert_eq!(timeline.query(0), Err(Status::NotFound));
}

#[test]
fn failed_gpu_completion_is_visible_and_retires_the_group() {
    let mut timeline = Timeline::default();
    timeline.reserve(1).unwrap();
    timeline.complete(1, 0).unwrap();
    timeline.reserve(2).unwrap();
    timeline.complete(2, 0x58).unwrap();
    assert_eq!(
        timeline.query(2),
        Ok(SyncState::Complete { gpu_status: 0x58 })
    );
    assert_eq!(timeline.query(1), Ok(SyncState::Complete { gpu_status: 0 }));
    assert_eq!(timeline.reserve(3), Err(Status::DeviceLost));
}

#[test]
fn prepublication_failure_rolls_back_without_fabricating_completion() {
    let mut timeline = Timeline::default();
    timeline.reserve(1).unwrap();
    let pending = timeline;
    assert_eq!(timeline.abort_unpublished(2), Err(Status::BadState));
    assert_eq!(timeline, pending);
    timeline.abort_unpublished(1).unwrap();
    assert_eq!(timeline, Timeline::default());
    assert_eq!(timeline.query(1), Err(Status::NotFound));
    timeline.reserve(1).unwrap();
    timeline.complete(1, 0).unwrap();
    assert_eq!(timeline.abort_unpublished(1), Err(Status::BadState));
}

#[test]
fn request_ids_reject_replay_and_never_wrap_into_an_old_session() {
    let mut ids = RequestIds::default();
    assert_eq!(ids.accept(0), Err(Status::BadRequest));
    ids.accept(1).unwrap();
    let before = ids;
    assert_eq!(ids.accept(1), Err(Status::BadRequest));
    assert_eq!(ids, before);
    ids.accept(3).unwrap();
    assert_eq!(ids.accept(2), Err(Status::BadRequest));
    ids.accept(u32::MAX).unwrap();
    assert_eq!(ids.accept(1), Err(Status::BadRequest));
    let mut timeline = Timeline {
        submitted: u64::MAX,
        completed: u64::MAX,
        gpu_status: 0,
    };
    assert_eq!(timeline.reserve(0), Err(Status::BadRequest));
}

#[test]
fn topology_preserves_all_64_core_ids_and_rejects_zero_or_noncanonical_frames() {
    let request = Request {
        id: 11,
        command: Command::GetTopology,
    };
    let bytes = request.encode();
    assert_eq!(&bytes[8..12], &8u32.to_le_bytes());
    for at in 12..REQUEST_LEN {
        let mut bad = bytes;
        bad[at] = 1;
        assert_eq!(Request::decode(&bad), None);
    }
    for shader_present in [1, 0xf, 0x500000005, 0x8000000000000001, u64::MAX] {
        let response = Response::Topology(DeviceTopology { shader_present });
        let reply = Reply {
            id: 11,
            result: Ok(response),
        };
        let bytes = reply.encode();
        assert_eq!(&bytes[12..16], &8u32.to_le_bytes());
        assert_eq!(&bytes[16..24], &shader_present.to_le_bytes());
        assert_eq!(Reply::decode(&bytes), Some(reply));
        assert_eq!(response.handle_count(), 0);
        for at in 24..REPLY_LEN {
            let mut bad = bytes;
            bad[at] = 1;
            assert_eq!(Reply::decode(&bad), None);
        }
    }
    let zero = Reply {
        id: 11,
        result: Ok(Response::Topology(DeviceTopology { shader_present: 0 })),
    };
    assert_eq!(Reply::decode(&zero.encode()), None);
}

#[test]
fn mmu_features_preserve_all_32_bits_and_reject_zero_truncation_or_reserved_bytes() {
    let request = Request {
        id: 12,
        command: Command::GetMmuFeatures,
    };
    let bytes = request.encode();
    assert_eq!(&bytes[8..12], &9u32.to_le_bytes());
    for at in 12..REQUEST_LEN {
        let mut bad = bytes;
        bad[at] = 1;
        assert_eq!(Request::decode(&bad), None);
    }
    for mmu_features in [1, 0x2830, 0x8000_0000, u32::MAX] {
        let response = Response::Mmu(DeviceMmu { mmu_features });
        let reply = Reply {
            id: 12,
            result: Ok(response),
        };
        let bytes = reply.encode();
        assert_eq!(&bytes[12..16], &9u32.to_le_bytes());
        assert_eq!(&bytes[16..24], &u64::from(mmu_features).to_le_bytes());
        assert_eq!(Reply::decode(&bytes), Some(reply));
        assert_eq!(response.handle_count(), 0);
        for at in 24..REPLY_LEN {
            let mut bad = bytes;
            bad[at] = 1;
            assert_eq!(Reply::decode(&bad), None);
        }
    }
    // A zero attestation is not a fallback address width; the frame must not decode. — SAKER
    let zero = Reply {
        id: 12,
        result: Ok(Response::Mmu(DeviceMmu { mmu_features: 0 })),
    };
    assert_eq!(Reply::decode(&zero.encode()), None);
    // Bits above the u32 field would narrow silently, so the canonical re-encode check must
    // reject the frame rather than accept a truncated value. — SAKER
    let mut widened = zero.encode();
    widened[16..24].copy_from_slice(&0x1_0000_2830u64.to_le_bytes());
    assert_eq!(Reply::decode(&widened), None);
}

#[test]
fn core_features_preserve_all_32_bits_including_the_measured_zero_word_and_reject_truncation_or_reserved_bytes(
) {
    let request = Request {
        id: 15,
        command: Command::GetCoreFeatures,
    };
    let bytes = request.encode();
    assert_eq!(&bytes[8..12], &12u32.to_le_bytes());
    for at in 12..REQUEST_LEN {
        let mut bad = bytes;
        bad[at] = 1;
        assert_eq!(Request::decode(&bad), None);
    }
    // The modeled word carries variant byte 0 (the pinned Mesa table's G610 requirement) plus
    // an unrelated bit above the byte, so the whole word must cross: [7:0] selection is the
    // consumer's job, and bits above the field would narrow silently. Zero is the board's
    // measured word (J650 metal: CORE_FEATURES=0x00000000), so it must round-trip too.
    // — TIMBERDOODLE
    for core_features in [0, 1, 0x0000_1000, 0x8000_0000, u32::MAX] {
        let response = Response::Core(DeviceCore { core_features });
        let reply = Reply {
            id: 15,
            result: Ok(response),
        };
        let bytes = reply.encode();
        assert_eq!(&bytes[12..16], &12u32.to_le_bytes());
        assert_eq!(&bytes[16..24], &u64::from(core_features).to_le_bytes());
        assert_eq!(Reply::decode(&bytes), Some(reply));
        assert_eq!(response.handle_count(), 0);
        for at in 24..REPLY_LEN {
            let mut bad = bytes;
            bad[at] = 1;
            assert_eq!(Reply::decode(&bad), None);
        }
    }
    // A word wider than 32 bits is a truncated frame, whatever the low bits say: the reply
    // re-encodes differently, so decode refuses it. — TIMBERDOODLE
    let zero = Reply {
        id: 15,
        result: Ok(Response::Core(DeviceCore { core_features: 0 })),
    };
    let mut widened = zero.encode();
    widened[16..24].copy_from_slice(&0x1_0000_1000u64.to_le_bytes());
    assert_eq!(Reply::decode(&widened), None);
}

#[test]
fn tiler_features_preserve_all_32_bits_and_reject_zero_truncation_or_reserved_bytes() {
    let request = Request {
        id: 13,
        command: Command::GetTilerFeatures,
    };
    let bytes = request.encode();
    assert_eq!(&bytes[8..12], &10u32.to_le_bytes());
    for at in 12..REQUEST_LEN {
        let mut bad = bytes;
        bad[at] = 1;
        assert_eq!(Request::decode(&bad), None);
    }
    // 0x809 is RESEARCH/009's predicted G610 word; upstream reads [4:0] as a log2 bin size
    // and [11:8] as the level count, so it must cross with every bit intact. — SAKER
    for tiler_features in [1, 0x809, 0x8000_0000, u32::MAX] {
        let response = Response::Tiler(DeviceTiler { tiler_features });
        let reply = Reply {
            id: 13,
            result: Ok(response),
        };
        let bytes = reply.encode();
        assert_eq!(&bytes[12..16], &10u32.to_le_bytes());
        assert_eq!(&bytes[16..24], &u64::from(tiler_features).to_le_bytes());
        assert_eq!(Reply::decode(&bytes), Some(reply));
        assert_eq!(response.handle_count(), 0);
        for at in 24..REPLY_LEN {
            let mut bad = bytes;
            bad[at] = 1;
            assert_eq!(Reply::decode(&bad), None);
        }
    }
    // Zero never crosses as a success: the backend degrades to NotSupported instead, so a
    // zero-word reply is a malformed frame rather than an "unknown tiler" answer. — SAKER
    let zero = Reply {
        id: 13,
        result: Ok(Response::Tiler(DeviceTiler { tiler_features: 0 })),
    };
    assert_eq!(Reply::decode(&zero.encode()), None);
    let mut widened = zero.encode();
    widened[16..24].copy_from_slice(&0x1_0000_0809u64.to_le_bytes());
    assert_eq!(Reply::decode(&widened), None);
}

#[test]
fn texture_features_carry_all_four_words_and_reject_zero_element_0_or_high_bits() {
    let request = Request {
        id: 14,
        command: Command::GetTextureFeatures,
    };
    let bytes = request.encode();
    assert_eq!(&bytes[8..12], &11u32.to_le_bytes());
    for at in 12..REQUEST_LEN {
        let mut bad = bytes;
        bad[at] = 1;
        assert_eq!(Request::decode(&bad), None);
    }
    // The reply carries the whole array, one word per u64 result slot. Element 0 is the
    // compressed-format mask with bit 25 as AFRC; elements 1..3 are attested verbatim, so a
    // zero among them is a real value, not absence, and both ends of every word must survive.
    // — TIMBERDOODLE
    for texture_features in [
        [1, 0, 0, 0],
        [0x0200_0000, 0xffff_ffff, 0, 0x8000_0000],
        [0x0200_0001, 0, 0x4000_0000, 0],
        [0x8000_0000, 1, 2, 3],
        [u32::MAX; 4],
    ] {
        let response = Response::Texture(DeviceTexture { texture_features });
        let reply = Reply {
            id: 14,
            result: Ok(response),
        };
        let bytes = reply.encode();
        assert_eq!(&bytes[12..16], &11u32.to_le_bytes());
        for (slot, word) in texture_features.iter().enumerate() {
            assert_eq!(
                &bytes[16 + slot * 8..24 + slot * 8],
                &u64::from(*word).to_le_bytes()
            );
        }
        assert_eq!(Reply::decode(&bytes), Some(reply));
        assert_eq!(response.handle_count(), 0);
    }
    // Every element occupies a u32 inside a u64 slot: a nonzero high half is a truncated
    // frame the re-encode guard refuses, wherever in the array it appears. — TIMBERDOODLE
    let reply = Reply {
        id: 14,
        result: Ok(Response::Texture(DeviceTexture {
            texture_features: [0x0200_0001, 0, 0x8000_0000, 0],
        })),
    };
    let bytes = reply.encode();
    for at in [20, 28, 36, 44] {
        let mut bad = bytes;
        bad[at] = 1;
        assert_eq!(Reply::decode(&bad), None, "high byte {at}");
    }
    let mut widened = bytes;
    widened[40..48].copy_from_slice(&0x1_0000_0000u64.to_le_bytes());
    assert_eq!(Reply::decode(&widened), None);
    // Element 0 is the session's optional-absence gate, so a zero there never crosses as a
    // success, even when elements 1..3 look attested. — TIMBERDOODLE
    let zero = Reply {
        id: 14,
        result: Ok(Response::Texture(DeviceTexture {
            texture_features: [0, 0xffff_ffff, 0xffff_ffff, 0xffff_ffff],
        })),
    };
    assert_eq!(Reply::decode(&zero.encode()), None);
}

#[test]
fn thread_properties_cross_three_raw_words_accept_all_zero_and_reject_truncation_or_reserved_slots()
{
    let request = Request {
        id: 16,
        command: Command::GetThreadProperties,
    };
    let bytes = request.encode();
    assert_eq!(&bytes[8..12], &13u32.to_le_bytes());
    for at in 12..REQUEST_LEN {
        let mut bad = bytes;
        bad[at] = 1;
        assert_eq!(Request::decode(&bad), None);
    }
    // Unlike tiler and texture features, all three words zero is a legitimate absence: the
    // backend decides the group is missing, the frame carries it verbatim, and C keeps
    // missing-property bit 4. So the zero triple must round-trip as a success here; only an
    // out-of-range word or a nonzero reserved slot is malformed. The other rows pin each
    // word independently, including 0x3fffff (the whole registers-per-core field), 255
    // (the whole tasks-per-core field shifted up) and both ends of u32. — TIMBERDOODLE
    for (max_threads, max_workgroup_size, thread_features) in [
        (0, 0, 0),
        (1024, 512, 0x0310_0005),
        (0, 512, 0x0310_0005),
        (1024, 0, 0x0310_0005),
        (1024, 512, 0),
        (1, 1, 0x003f_ffff),
        (u32::MAX, u32::MAX, u32::MAX),
        (0, 0, 0x8000_0000),
    ] {
        let response = Response::Threads(DeviceThreads {
            max_threads,
            max_workgroup_size,
            thread_features,
        });
        let reply = Reply {
            id: 16,
            result: Ok(response),
        };
        let bytes = reply.encode();
        assert_eq!(&bytes[12..16], &13u32.to_le_bytes());
        assert_eq!(&bytes[16..24], &u64::from(max_threads).to_le_bytes());
        assert_eq!(&bytes[24..32], &u64::from(max_workgroup_size).to_le_bytes());
        assert_eq!(&bytes[32..40], &u64::from(thread_features).to_le_bytes());
        assert_eq!(&bytes[40..48], &0u64.to_le_bytes());
        assert_eq!(Reply::decode(&bytes), Some(reply));
        assert_eq!(response.handle_count(), 0);
        // Only slot 3 is reserved; slots 0..2 may carry any word, zero included.
        for at in 40..REPLY_LEN {
            let mut bad = bytes;
            bad[at] = 1;
            assert_eq!(Reply::decode(&bad), None);
        }
    }
    // A word wider than 32 bits would narrow silently, so the canonical re-encode check must
    // refuse each of the three slots separately. — TIMBERDOODLE
    for slot in 0..3 {
        let mut widened = Reply {
            id: 16,
            result: Ok(Response::Threads(DeviceThreads {
                max_threads: 1024,
                max_workgroup_size: 512,
                thread_features: 0x0310_0005,
            })),
        }
        .encode();
        let at = 16 + slot * 8;
        widened[at..at + 8].copy_from_slice(&(0x1_0000_0000u64 + 0x1234).to_le_bytes());
        assert_eq!(Reply::decode(&widened), None);
    }
    // A nonzero reserved slot is not an extension point; decode refuses it. — TIMBERDOODLE
    let mut reserved = Reply {
        id: 16,
        result: Ok(Response::Threads(DeviceThreads {
            max_threads: 1024,
            max_workgroup_size: 512,
            thread_features: 0x0310_0005,
        })),
    }
    .encode();
    reserved[40..48].copy_from_slice(&1u64.to_le_bytes());
    assert_eq!(Reply::decode(&reserved), None);
}
