use std::collections::HashSet;
use std::time::Instant;

use log::info;

use crate::circuit_data::{CircuitConfig, CircuitData, CommonCircuitData, ProverCircuitData, ProverOnlyCircuitData, VerifierCircuitData, VerifierOnlyCircuitData};
use crate::field::fft::lde_multiple;
use crate::field::field::Field;
use crate::gates::constant::ConstantGate2;
use crate::gates::gate::{GateInstance, GateRef};
use crate::generator::{CopyGenerator, WitnessGenerator};
use crate::hash::merkle_root_bit_rev_order;
use crate::target::Target;
use crate::wire::Wire;
use crate::gates::noop::NoopGate;
use crate::util::transpose;

pub struct CircuitBuilder<F: Field> {
    config: CircuitConfig,

    /// The types of gates used in this circuit.
    gates: HashSet<GateRef<F>>,

    gate_instances: Vec<GateInstance<F>>,

    generators: Vec<Box<dyn WitnessGenerator<F>>>,
}

impl<F: Field> CircuitBuilder<F> {
    pub fn new(config: CircuitConfig) -> Self {
        CircuitBuilder {
            config,
            gates: HashSet::new(),
            gate_instances: Vec::new(),
            generators: Vec::new(),
        }
    }

    pub fn add_gate_no_constants(&mut self, gate_type: GateRef<F>) -> usize {
        self.add_gate(gate_type, Vec::new())
    }

    /// Adds a gate to the circuit, and returns its index.
    pub fn add_gate(&mut self, gate_type: GateRef<F>, constants: Vec<F>) -> usize {
        // If we haven't seen a gate of this type before, check that it's compatible with our
        // circuit configuration, then register it.
        if !self.gates.contains(&gate_type) {
            self.check_gate_compatibility(&gate_type);
            self.gates.insert(gate_type.clone());
        }

        let index = self.gate_instances.len();
        self.gate_instances.push(GateInstance { gate_type, constants });
        index
    }

    fn check_gate_compatibility(&self, gate: &GateRef<F>) {
        assert!(gate.0.min_wires(self.config) <= self.config.num_wires);
    }

    /// Shorthand for `generate_copy` and `assert_equal`.
    /// Both elements must be routable, otherwise this method will panic.
    pub fn route(&mut self, src: Target, dst: Target) {
        self.generate_copy(src, dst);
        self.assert_equal(src, dst);
    }

    /// Adds a generator which will copy `src` to `dst`.
    pub fn generate_copy(&mut self, src: Target, dst: Target) {
        self.add_generator(CopyGenerator { src, dst });
    }

    /// Uses Plonk's permutation argument to require that two elements be equal.
    /// Both elements must be routable, otherwise this method will panic.
    pub fn assert_equal(&mut self, x: Target, y: Target) {
        assert!(x.is_routable(self.config));
        assert!(y.is_routable(self.config));
    }

    pub fn add_generator<G: WitnessGenerator<F>>(&mut self, generator: G) {
        self.generators.push(Box::new(generator));
    }

    /// Returns a routable target with a value of 0.
    pub fn zero(&mut self) -> Target {
        self.constant(F::ZERO)
    }

    /// Returns a routable target with a value of 1.
    pub fn one(&mut self) -> Target {
        self.constant(F::ONE)
    }

    /// Returns a routable target with a value of 2.
    pub fn two(&mut self) -> Target {
        self.constant(F::TWO)
    }

    /// Returns a routable target with a value of `ORDER - 1`.
    pub fn neg_one(&mut self) -> Target {
        self.constant(F::NEG_ONE)
    }

    /// Returns a routable target with the given constant value.
    pub fn constant(&mut self, c: F) -> Target {
        let gate = self.add_gate(ConstantGate2::get(), vec![c]);
        Target::Wire(Wire { gate, input: ConstantGate2::WIRE_OUTPUT })
    }

    fn blind_and_pad(&mut self) {
        // TODO: Blind.

        while !self.gate_instances.len().is_power_of_two() {
            self.add_gate_no_constants(NoopGate::get());
        }
    }

    fn get_generators(&self) -> Vec<Box<dyn WitnessGenerator<F>>> {
        self.gate_instances.iter()
            .enumerate()
            .flat_map(|(gate_index, gate_inst)| gate_inst.gate_type.0.generators(
                self.config,
                gate_index,
                gate_inst.constants.clone(),
                vec![])) // TODO: Not supporting next_const for now.
            .collect()
    }

    fn constant_vecs(&self) -> Vec<Vec<F>> {
        let num_constants = self.gate_instances.iter()
            .map(|gate_inst| gate_inst.constants.len())
            .max()
            .unwrap();
        let constants_per_gate = self.gate_instances.iter()
            .map(|gate_inst| {
                let mut padded_constants = gate_inst.constants.clone();
                for _ in padded_constants.len()..num_constants {
                    padded_constants.push(F::ZERO);
                }
                padded_constants
            })
            .collect::<Vec<_>>();
        transpose(&constants_per_gate)
    }

    fn sigma_vecs(&self) -> Vec<Vec<F>> {
        vec![vec![F::ZERO]] // TODO
    }

    /// Builds a "full circuit", with both prover and verifier data.
    pub fn build(mut self) -> CircuitData<F> {
        let start = Instant::now();
        info!("degree before blinding & padding: {}", self.gate_instances.len());
        self.blind_and_pad();
        let degree = self.gate_instances.len();
        info!("degree after blinding & padding: {}", degree);

        let constant_vecs = self.constant_vecs();
        let constant_ldes = lde_multiple(constant_vecs, self.config.rate_bits);
        let constants_root = merkle_root_bit_rev_order(constant_ldes);

        let sigma_vecs = self.sigma_vecs();
        let sigma_ldes = lde_multiple(sigma_vecs, self.config.rate_bits);
        let sigmas_root = merkle_root_bit_rev_order(sigma_ldes);

        let generators = self.get_generators();
        let prover_only = ProverOnlyCircuitData { generators };
        let verifier_only = VerifierOnlyCircuitData {};

        let common = CommonCircuitData {
            config: self.config,
            degree,
            gates: self.gates.iter().cloned().collect(),
            constants_root,
            sigmas_root,
        };

        info!("Building circuit took {}s", start.elapsed().as_secs_f32());
        CircuitData {
            prover_only,
            verifier_only,
            common,
        }
    }

    /// Builds a "prover circuit", with data needed to generate proofs but not verify them.
    pub fn build_prover(mut self) -> ProverCircuitData<F> {
        // TODO: Can skip parts of this.
        let CircuitData { prover_only, verifier_only, common } = self.build();
        ProverCircuitData { prover_only, common }
    }

    /// Builds a "verifier circuit", with data needed to verify proofs but not generate them.
    pub fn build_verifier(mut self) -> VerifierCircuitData<F> {
        // TODO: Can skip parts of this.
        let CircuitData { prover_only, verifier_only, common } = self.build();
        VerifierCircuitData { verifier_only, common }
    }
}
