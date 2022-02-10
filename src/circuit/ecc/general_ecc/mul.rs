use super::AssignedPoint;
use crate::circuit::ecc::general_ecc::GeneralEccChip;
use crate::circuit::ecc::{Selector, Table, Windowed};
use crate::circuit::{AssignedInteger, IntegerInstructions};
use halo2::arithmetic::{CurveAffine, FieldExt};
use halo2::circuit::Region;
use halo2::plonk::Error;
use halo2arith::{halo2, AssignedCondition, MainGateInstructions};

impl<Emulated: CurveAffine, F: FieldExt> GeneralEccChip<Emulated, F> {
    fn pad(&self, region: &mut Region<'_, F>, bits: &mut Vec<AssignedCondition<F>>, window_size: usize, offset: &mut usize) -> Result<(), Error> {
        use group::ff::PrimeField;
        assert_eq!(bits.len(), Emulated::ScalarExt::NUM_BITS as usize);

        // TODO: This is a tmp workaround. Instead of padding with zeros we can use a shorter ending window.
        let padding_offset = (window_size - (bits.len() % window_size)) % window_size;
        let zeros: Vec<AssignedCondition<F>> = (0..padding_offset)
            .map(|_| Ok(self.main_gate().assign_constant(region, F::zero(), offset)?.into()))
            .collect::<Result<_, Error>>()?;
        bits.extend(zeros);
        bits.reverse();

        Ok(())
    }

    fn window(bits: Vec<AssignedCondition<F>>, window_size: usize) -> Windowed<F> {
        assert_eq!(bits.len() % window_size, 0);
        let number_of_windows = bits.len() / window_size;
        Windowed(
            (0..number_of_windows)
                .map(|i| {
                    let mut selector: Vec<AssignedCondition<F>> = (0..window_size).map(|j| bits[i * window_size + j].clone()).collect();
                    selector.reverse();
                    Selector(selector)
                })
                .collect(),
        )
    }

    fn make_incremental_table(
        &self,
        region: &mut Region<'_, F>,
        aux: &AssignedPoint<F>,
        point: &AssignedPoint<F>,
        window_size: usize,
        offset: &mut usize,
    ) -> Result<Table<F>, Error> {
        let table_size = 1 << window_size;
        let mut table = vec![aux.clone()];
        for i in 0..(table_size - 1) {
            table.push(self.add(region, &table[i], point, offset)?);
        }
        Ok(Table(table))
    }

    fn select_multi(&self, region: &mut Region<'_, F>, selector: &Selector<F>, table: &Table<F>, offset: &mut usize) -> Result<AssignedPoint<F>, Error> {
        let number_of_points = table.0.len();
        let number_of_selectors = selector.0.len();
        assert_eq!(number_of_points, 1 << number_of_selectors);

        let mut reducer = table.0.clone();
        for (i, selector) in selector.0.iter().enumerate() {
            let n = 1 << (number_of_selectors - 1 - i);
            for j in 0..n {
                let k = 2 * j;
                reducer[j] = self.select(region, selector, &reducer[k + 1], &reducer[k], offset)?;
            }
        }
        Ok(reducer[0].clone())
    }

    pub fn mul(
        &self,
        region: &mut Region<'_, F>,
        point: &AssignedPoint<F>,
        scalar: &AssignedInteger<F>,
        window_size: usize,
        offset: &mut usize,
    ) -> Result<AssignedPoint<F>, Error> {
        assert!(window_size > 0);
        let aux = self.get_mul_aux(window_size, 1)?;

        let scalar_chip = self.scalar_field_chip();
        let decomposed = &mut scalar_chip.decompose(region, scalar, offset)?;
        self.pad(region, decomposed, window_size, offset)?;
        let windowed = Self::window(decomposed.to_vec(), window_size);
        let table = &self.make_incremental_table(region, &aux.to_add, point, window_size, offset)?;

        let mut acc = self.select_multi(region, &windowed.0[0], table, offset)?;
        acc = self.double_n(region, &acc, window_size, offset)?;

        let to_add = self.select_multi(region, &windowed.0[1], table, offset)?;
        acc = self.add(region, &acc, &to_add, offset)?;

        for selector in windowed.0.iter().skip(2) {
            acc = self.double_n(region, &acc, window_size - 1, offset)?;
            let to_add = self.select_multi(region, selector, table, offset)?;
            acc = self.ladder(region, &acc, &to_add, offset)?;
        }

        self.add(region, &acc, &aux.to_sub, offset)
    }

    pub fn mul_batch_1d_horizontal(
        &self,
        region: &mut Region<'_, F>,
        pairs: Vec<(AssignedPoint<F>, AssignedInteger<F>)>,
        window_size: usize,
        offset: &mut usize,
    ) -> Result<AssignedPoint<F>, Error> {
        assert!(window_size > 0);
        assert!(pairs.len() > 0);
        let aux = self.get_mul_aux(window_size, pairs.len())?;

        let scalar_chip = self.scalar_field_chip();
        let mut decomposed_scalars: Vec<Vec<AssignedCondition<F>>> = pairs
            .iter()
            .map(|(_, scalar)| scalar_chip.decompose(region, scalar, offset))
            .collect::<Result<_, Error>>()?;

        for decomposed in decomposed_scalars.iter_mut() {
            self.pad(region, decomposed, window_size, offset)?;
        }

        let windowed_scalars: Vec<Windowed<F>> = decomposed_scalars
            .iter()
            .map(|decomposed| Self::window(decomposed.to_vec(), window_size))
            .collect();
        let number_of_windows = windowed_scalars[0].0.len();

        let mut binary_aux = aux.to_add.clone();
        let tables: Vec<Table<F>> = pairs
            .iter()
            .enumerate()
            .map(|(i, (point, _))| {
                let table = self.make_incremental_table(region, &binary_aux, point, window_size, offset);
                if i != pairs.len() - 1 {
                    binary_aux = self.double(region, &binary_aux, offset)?;
                }
                table
            })
            .collect::<Result<_, Error>>()?;

        // preparation for the first round
        // initialize accumulator
        let mut acc = self.select_multi(region, &windowed_scalars[0].0[0], &tables[0], offset)?;
        // add first contributions other point scalar
        for (table, windowed) in tables.iter().skip(1).zip(windowed_scalars.iter().skip(1)) {
            let selector = &windowed.0[0];
            let to_add = self.select_multi(region, selector, table, offset)?;
            acc = self.add(region, &acc, &to_add, offset)?;
        }

        for i in 1..number_of_windows {
            acc = self.double_n(region, &acc, window_size, offset)?;
            for (table, windowed) in tables.iter().zip(windowed_scalars.iter()) {
                let selector = &windowed.0[i];
                let to_add = self.select_multi(region, selector, table, offset)?;
                acc = self.add(region, &acc, &to_add, offset)?;
            }
        }

        self.add(region, &acc, &aux.to_sub, offset)
    }
}
