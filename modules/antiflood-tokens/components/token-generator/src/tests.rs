use super::*;

mod token_assignment {
    use super::*;
    use chrono::{NaiveDate, Timelike};
    use ed25519_dalek::{PublicKey, Signature};
    use locutus_aft_interface::Tier;
    use once_cell::sync::Lazy;

    fn get_assignment_date(y: i32, m: u32, d: u32) -> DateTime<Utc> {
        let naive = NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap();
        DateTime::<Utc>::from_utc(naive, Utc)
    }

    const TEST_TIER: Tier = Tier::Day1;
    const MAX_DURATION_1Y: std::time::Duration = std::time::Duration::from_secs(365 * 24 * 3600);

    static PK: Lazy<PublicKey> =
        Lazy::new(|| PublicKey::from_bytes(&[1; ed25519_dalek::PUBLIC_KEY_LENGTH]).unwrap());

    static ID: Lazy<ContractInstanceId> = Lazy::new(|| {
        let rnd = [1; 32];
        let mut gen = arbitrary::Unstructured::new(&rnd);
        gen.arbitrary().unwrap()
    });

    #[test]
    fn free_spot_first() {
        let records = TokenAllocationRecord::new(HashMap::from_iter([(
            TEST_TIER,
            vec![TokenAssignment {
                tier: TEST_TIER,
                time_slot: get_assignment_date(2023, 1, 25),
                assignee: *PK,
                signature: Signature::from([1; 64]),
                assignment_hash: [0; 32],
                token_record: *ID,
            }],
        )]));
        let assignment = records.next_free_assignment(
            &AllocationCriteria::new(TEST_TIER, MAX_DURATION_1Y, *ID).unwrap(),
            get_assignment_date(2023, 1, 27),
        );
        assert_eq!(assignment.unwrap(), get_assignment_date(2022, 1, 27));
    }

    #[test]
    fn free_spot_skip_first() {
        let records = TokenAllocationRecord::new(HashMap::from_iter([(
            TEST_TIER,
            vec![
                TokenAssignment {
                    tier: TEST_TIER,
                    time_slot: get_assignment_date(2022, 1, 27),
                    assignee: *PK,
                    signature: Signature::from([1; 64]),
                    assignment_hash: [0; 32],
                    token_record: *ID,
                },
                TokenAssignment {
                    tier: TEST_TIER,
                    time_slot: get_assignment_date(2023, 1, 26),
                    assignee: *PK,
                    signature: Signature::from([1; 64]),
                    assignment_hash: [0; 32],
                    token_record: *ID,
                },
            ],
        )]));
        let assignment = records.next_free_assignment(
            &AllocationCriteria::new(TEST_TIER, MAX_DURATION_1Y, *ID).unwrap(),
            get_assignment_date(2023, 1, 27).with_minute(1).unwrap(),
        );
        assert_eq!(assignment.unwrap(), get_assignment_date(2022, 1, 28));
    }

    #[test]
    fn free_spot_skip_gap_1() {
        let records = TokenAllocationRecord::new(HashMap::from_iter([(
            TEST_TIER,
            vec![
                TokenAssignment {
                    tier: TEST_TIER,
                    time_slot: get_assignment_date(2022, 1, 27),
                    assignee: *PK,
                    signature: Signature::from([1; 64]),
                    assignment_hash: [0; 32],
                    token_record: *ID,
                },
                TokenAssignment {
                    tier: TEST_TIER,
                    time_slot: get_assignment_date(2022, 1, 29),
                    assignee: *PK,
                    signature: Signature::from([1; 64]),
                    assignment_hash: [0; 32],
                    token_record: *ID,
                },
            ],
        )]));
        let assignment = records.next_free_assignment(
            &AllocationCriteria::new(TEST_TIER, MAX_DURATION_1Y, *ID).unwrap(),
            get_assignment_date(2023, 1, 27),
        );
        assert_eq!(assignment.unwrap(), get_assignment_date(2022, 1, 28));

        let records = TokenAllocationRecord::new(HashMap::from_iter([(
            TEST_TIER,
            vec![
                TokenAssignment {
                    tier: TEST_TIER,
                    time_slot: get_assignment_date(2022, 1, 27),
                    assignee: *PK,
                    signature: Signature::from([1; 64]),
                    assignment_hash: [0; 32],
                    token_record: *ID,
                },
                TokenAssignment {
                    tier: TEST_TIER,
                    time_slot: get_assignment_date(2022, 1, 28),
                    assignee: *PK,
                    signature: Signature::from([1; 64]),
                    assignment_hash: [0; 32],
                    token_record: *ID,
                },
                TokenAssignment {
                    tier: TEST_TIER,
                    time_slot: get_assignment_date(2022, 1, 30),
                    assignee: *PK,
                    signature: Signature::from([1; 64]),
                    assignment_hash: [0; 32],
                    token_record: *ID,
                },
            ],
        )]));
        let assignment = records.next_free_assignment(
            &AllocationCriteria::new(TEST_TIER, MAX_DURATION_1Y, *ID).unwrap(),
            get_assignment_date(2023, 1, 27).with_minute(1).unwrap(),
        );
        assert_eq!(assignment.unwrap(), get_assignment_date(2022, 1, 29));
    }

    #[test]
    fn free_spot_new() {
        let records = TokenAllocationRecord::new(HashMap::new());
        let assignment = records.next_free_assignment(
            &AllocationCriteria::new(TEST_TIER, MAX_DURATION_1Y, *ID).unwrap(),
            get_assignment_date(2023, 1, 27).with_minute(1).unwrap(),
        );
        assert_eq!(assignment.unwrap(), get_assignment_date(2022, 1, 28));
    }
}
