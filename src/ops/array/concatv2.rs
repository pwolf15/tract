use analyser::interface::*;
use ndarray::prelude::*;
use ops::prelude::*;

pub fn build(pb: &::tfpb::node_def::NodeDef) -> Result<Box<Op>> {
    let n = pb.get_attr_int("N")?;
    let t = pb.get_attr_datum_type("T")?;
    let tidx = pb.get_attr_datum_type("Tidx")?;
    Ok(boxed_new!(ConcatV2(t)(n, tidx)))
}

#[derive(Debug, Clone, new)]
pub struct ConcatV2<T: Datum> {
    n: usize,
    tidx: DatumType,
    t: PhantomData<T>,
}

impl<T: Datum> Op for ConcatV2<T> {
    /// Returns the attributes of the operation and their values.
    fn get_attributes(&self) -> HashMap<&'static str, Attr> {
        hashmap!{
            "n" => Attr::Usize(self.n),
            "t" => Attr::DatumType(T::datum_type()),
            "tidx" => Attr::DatumType(self.tidx),
        }
    }

    /// Evaluates the operation given the input tensors.
    fn eval(&self, mut inputs: Vec<Value>) -> Result<Vec<Value>> {
        let axis: i32 = inputs.pop().and_then(|t| t.as_i32()).ok_or("Expected a i32 scalar")?;
        let mats: Result<Vec<ArrayViewD<T>>> =
            inputs.iter().map(|mat| T::tensor_to_view(&mat)).collect();
        let result = ::ndarray::stack(Axis(axis as usize), &*mats?)?;
        let result = T::array_into_tensor(result);

        Ok(vec![result.into()])
    }

    /// Returns a new streaming buffer for the operation.
    fn new_buffer(&self) -> Box<OpBuffer> {
        Box::new(QueuesBuffer::new(self.n))
    }

    /// Evaluates one step of the operation on the given input tensors.
    fn step(
        &self,
        mut inputs: Vec<StepValue>,
        buffer: &mut Box<OpBuffer>,
    ) -> Result<Option<Vec<Value>>> {
        // According to https://www.tensorflow.org/api_docs/python/tf/concat,
        // the number of dimensions of each input tensor must match, and all
        // dimensions except `axis` must be equal. In particular, this means
        // that all the input tensors must have the same streaming dimension.
        // That leaves us with two cases:
        // - Either all the tensors are streamed along `axis`, in which case
        //   we push every slice we receive as input directly to the output.
        // - Or they are streamed along another dimension, so we buffer them
        //   until we have a chunk for each, and we push their concatenation
        //   as the output chunk.

        let n = inputs.pop().ok_or("Unexpectedly found zero inputs in ConcatV2")?;
        let axis_tensor = n.into_const().ok_or("Axis input should not be streamed.")?;
        let axis: i32 = axis_tensor.as_i32().ok_or("Expected a i32 scalar")?;

        if inputs.iter().all(|i| i.streaming_dim() == Some(axis as usize)) {
            // All the input tensors are streamed along `axis`.
            let chunk = inputs
                .into_iter()
                .map(|sv| sv.into_value().ok_or("Expected a value".into()))
                .collect::<Result<Vec<Value>>>()?;

            Ok(Some(chunk))
        } else {
            // All the input tensors are streamed along a non-`axis` dimension.
            let buffer = buffer
                .downcast_mut::<QueuesBuffer>()
                .ok_or("The buffer can't be downcasted to QueuesBuffer.")?;

            buffer.append(inputs)?;

            if buffer.iter_mut().any(|q| q.is_empty()) {
                Ok(None)
            } else {
                let mut chunks = buffer
                    .iter_mut()
                    .map(|b| b.pop_front().unwrap())
                    .collect::<Vec<_>>();

                chunks.push(axis_tensor);

                Ok(Some(self.eval(chunks)?))
            }
        }
    }
}

impl<T: Datum> InferenceRulesOp for ConcatV2<T> {
    fn rules<'r, 'p: 'r, 's: 'r>(
        &'s self,
        solver: &mut Solver<'r>,
        inputs: &'p TensorsProxy,
        outputs: &'p TensorsProxy,
    ) {
        let n = self.n;
        solver
            .equals(&inputs.len, n as isize + 1)
            .equals(&outputs.len, 1)
            .equals_all((0..self.n).map(|i| bexp(&inputs[i].datum_type)).collect())
            .equals(&outputs[0].datum_type, &inputs[0].datum_type)
            .equals(&inputs[n].datum_type, DatumType::I32)
            .equals_all((0..self.n).map(|i| bexp(&inputs[i].rank)).collect())
            .equals(&inputs[n].rank, 0)
            .equals(&outputs[0].rank, &inputs[0].rank)
            .given(&inputs[n].value, move |solver, axis: Tensor| {
                let axis = axis.as_i32().unwrap() as usize; // checked
                trace!("axis for Concatv2: {}", axis);
                (0..axis).for_each(|d| {
                    solver.equals_all((0..n).map(|i| bexp(&inputs[i].shape[d])).collect());
                });
                (0..axis).for_each(|d| {
                    solver.equals(&inputs[0].shape[d], &outputs[0].shape[d]);
                });
                solver.given(&inputs[0].rank, move |solver, rank: usize| {
                    trace!("Given rank {}", rank);
                    ((axis + 1)..rank).for_each(|d| {
                        solver.equals(&inputs[0].shape[d], &outputs[0].shape[d]);
                    });
                    ((axis + 1)..rank).for_each(|d| {
                        solver.equals_all((0..n).map(|i| bexp(&inputs[i].shape[d])).collect());
                    });
                });

                let mut concat_dim = vec![bexp((-1, &outputs[0].shape[axis]))];
                concat_dim.extend((0..n).map(|i| bexp((1, &inputs[i].shape[axis]))));
                solver.equals_zero(concat_dim);
            });
    }
}
