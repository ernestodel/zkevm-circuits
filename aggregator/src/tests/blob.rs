use crate::{
    aggregation::{
        AssignedBarycentricEvaluationConfig, BarycentricEvaluationConfig, BlobDataConfig, RlcConfig,
    },
    blob::{BlobAssignments, BlobData, N_ROWS_DATA},
    param::ConfigParams,
    MAX_AGG_SNARKS,
};
use halo2_base::{
    gates::range::{RangeConfig, RangeStrategy},
    Context, ContextParams,
};
use halo2_proofs::{
    circuit::{AssignedCell, Layouter, SimpleFloorPlanner},
    dev::{MockProver, VerifyFailure},
    halo2curves::bn256::Fr,
    plonk::{Circuit, ConstraintSystem, Error},
};
use zkevm_circuits::{
    table::{KeccakTable, RangeTable, U8Table},
    util::Challenges,
};

struct BlobCircuit {
    data: BlobData,
}

#[derive(Clone, Debug)]
struct BlobConfig {
    challenges: Challenges,

    keccak_table: KeccakTable,

    rlc: RlcConfig,
    blob_data: BlobDataConfig,
    barycentric: BarycentricEvaluationConfig,
}

impl Circuit<Fr> for BlobCircuit {
    type Config = BlobConfig;
    type FloorPlanner = SimpleFloorPlanner;
    fn without_witnesses(&self) -> Self {
        unimplemented!()
    }

    fn configure(meta: &mut ConstraintSystem<Fr>) -> Self::Config {
        let u8_table = U8Table::construct(meta);
        let range_table = RangeTable::construct(meta);
        let challenges = Challenges::construct(meta);
        let keccak_table = KeccakTable::construct(meta);

        let rlc = RlcConfig::configure(meta, challenges);

        let parameters = ConfigParams::aggregation_param();
        let range = RangeConfig::<Fr>::configure(
            meta,
            RangeStrategy::Vertical,
            &parameters.num_advice,
            &parameters.num_lookup_advice,
            parameters.num_fixed,
            parameters.lookup_bits,
            0,
            parameters.degree.try_into().unwrap(),
        );
        let barycentric = BarycentricEvaluationConfig::construct(range);

        let challenge_expressions = challenges.exprs(meta);
        let blob_data = BlobDataConfig::configure(
            meta,
            challenge_expressions,
            u8_table,
            range_table,
            &keccak_table,
        );

        BlobConfig {
            challenges,

            keccak_table,

            rlc,
            blob_data,
            barycentric,
        }
    }

    fn synthesize(
        &self,
        config: Self::Config,
        mut layouter: impl Layouter<Fr>,
    ) -> Result<(), Error> {
        let challenge_values = config.challenges.values(&layouter);

        config
            .keccak_table
            .dev_load(&mut layouter, &self.data.preimages(), &challenge_values)?;

        let mut first_pass = halo2_base::SKIP_FIRST_PASS;
        let barycentric_assignments = layouter.assign_region(
            || "barycentric config",
            |region| -> Result<AssignedBarycentricEvaluationConfig, Error> {
                if first_pass {
                    first_pass = false;
                    return Ok(AssignedBarycentricEvaluationConfig::default());
                }

                let gate = &config.barycentric.scalar.range.gate;
                let mut ctx = Context::new(
                    region,
                    ContextParams {
                        max_rows: gate.max_rows,
                        num_context_ids: 1,
                        fixed_columns: gate.constants.clone(),
                    },
                );

                let blob = BlobAssignments::from(&self.data);
                Ok(config.barycentric.assign(
                    &mut ctx,
                    &blob.coefficients,
                    blob.challenge_digest,
                    blob.evaluation,
                ))
            },
        )?;

        let chunks_are_padding = layouter.assign_region(
            || "dev: chunks are padding or not",
            |mut region| -> Result<Vec<AssignedCell<Fr, Fr>>, Error> {
                let rlc_config = &config.rlc;
                rlc_config.init(&mut region)?;
                let mut rlc_config_offset = 0;

                let mut chunks_are_padding = Vec::with_capacity(MAX_AGG_SNARKS);
                for i in 0..MAX_AGG_SNARKS {
                    let is_padding = (i as u16) >= self.data.num_valid_chunks;
                    chunks_are_padding.push(rlc_config.load_private(
                        &mut region,
                        &Fr::from(is_padding as u64),
                        &mut rlc_config_offset,
                    )?);
                }

                Ok(chunks_are_padding)
            },
        )?;

        config.blob_data.assign(
            &mut layouter,
            challenge_values,
            &config.rlc,
            &chunks_are_padding,
            &self.data,
            &barycentric_assignments.barycentric_assignments,
        )?;

        Ok(())
    }
}

fn check_circuit(data: BlobData) -> Result<(), Vec<VerifyFailure>> {
    let k = 20;
    let mock_prover =
        MockProver::<Fr>::run(k, &BlobCircuit { data }, vec![]).expect("failed to run mock prover");
    mock_prover.verify_par()
}

#[test]
fn blob_circuit_completeness() {
    // single chunk in batch, but the chunk has a size of N_ROWS_DATA
    let full_blob = vec![vec![123; N_ROWS_DATA]];
    let all_empty_chunks: Vec<Vec<u8>> = vec![vec![]; MAX_AGG_SNARKS];
    let one_chunk = vec![vec![2, 3, 4, 100, 1]];
    let two_chunks = vec![vec![100; 1000], vec![2, 3, 4, 100, 1]];
    let max_chunks: Vec<Vec<u8>> = (0..MAX_AGG_SNARKS)
        .map(|i| (10u8..10 + u8::try_from(i).unwrap()).collect())
        .collect();
    let empty_chunk_followed_by_nonempty_chunk = vec![vec![], vec![3, 100, 24, 30]];
    let nonempty_chunk_followed_by_empty_chunk = vec![vec![3, 100, 24, 30], vec![]];
    let empty_and_nonempty_chunks = vec![
        vec![3, 100, 24, 30],
        vec![],
        vec![],
        vec![100, 23, 34, 24, 10],
        vec![],
    ];
    let all_empty_except_last = std::iter::repeat(vec![])
        .take(MAX_AGG_SNARKS - 1)
        .chain(std::iter::once(vec![3, 100, 24, 30]))
        .collect::<Vec<_>>();

    for blob in [
        full_blob,
        one_chunk,
        two_chunks,
        max_chunks,
        all_empty_chunks,
        empty_chunk_followed_by_nonempty_chunk,
        nonempty_chunk_followed_by_empty_chunk,
        empty_and_nonempty_chunks,
        all_empty_except_last,
    ] {
        assert_eq!(check_circuit(BlobData::from(&blob)), Ok(()), "{:?}", blob);
    }
}

fn generic_blob_data() -> BlobData {
    BlobData::from(&vec![
        vec![3, 100, 24, 30],
        vec![],
        vec![100; 300],
        vec![100, 23, 34, 24, 10],
        vec![200; 20],
        vec![],
        vec![200; 20],
    ])
}

#[test]
fn inconsistent_chunk_size() {
    let mut blob_data = generic_blob_data();
    blob_data.chunk_sizes[4] += 1;
    assert!(check_circuit(blob_data).is_err());
}

#[test]
fn too_many_empty_chunks() {
    let mut blob_data = generic_blob_data();
    blob_data.num_valid_chunks += 1;
    assert!(check_circuit(blob_data).is_err());
}

#[test]
fn too_few_empty_chunks() {
    let mut blob_data = generic_blob_data();
    blob_data.num_valid_chunks -= 1;
    assert!(check_circuit(blob_data).is_err());
}

#[test]
fn inconsistent_chunk_bytes() {
    let mut blob_data = generic_blob_data();
    blob_data.chunk_data[0].push(128);
    assert!(check_circuit(blob_data).is_err());
}