//! The `GeneralizedInput` is an input that ca be generalized to represent a rule, used by Grimoire

use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use crate::{
    corpus::{Corpus, CorpusId, Testcase},
    impl_serdeany,
    inputs::BytesInput,
    stages::mutational::{MutatedTransform, MutatedTransformPost},
    state::{HasCorpus, HasMetadata},
    Error,
};

/// An item of the generalized input
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Hash)]
pub enum GeneralizedItem {
    /// Real bytes
    Bytes(Vec<u8>),
    /// An insertion point
    Gap,
}

/// Metadata regarding the generalised content of an input
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct GeneralizedInputMetadata {
    generalized: Vec<GeneralizedItem>,
}

impl_serdeany!(GeneralizedInputMetadata);

impl GeneralizedInputMetadata {
    /// Fill the generalized vector from a slice of option (None -> Gap)
    #[must_use]
    pub fn generalized_from_options(v: &[Option<u8>]) -> Self {
        let mut generalized = vec![];
        let mut bytes = vec![];
        if v.first() != Some(&None) {
            generalized.push(GeneralizedItem::Gap);
        }
        for e in v {
            match e {
                None => {
                    if !bytes.is_empty() {
                        generalized.push(GeneralizedItem::Bytes(bytes.clone()));
                        bytes.clear();
                    }
                    generalized.push(GeneralizedItem::Gap);
                }
                Some(b) => {
                    bytes.push(*b);
                }
            }
        }
        if !bytes.is_empty() {
            generalized.push(GeneralizedItem::Bytes(bytes));
        }
        if generalized.last() != Some(&GeneralizedItem::Gap) {
            generalized.push(GeneralizedItem::Gap);
        }
        Self { generalized }
    }

    /// Get the size of the generalized
    #[must_use]
    pub fn generalized_len(&self) -> usize {
        let mut size = 0;
        for item in &self.generalized {
            match item {
                GeneralizedItem::Bytes(b) => size += b.len(),
                GeneralizedItem::Gap => size += 1,
            }
        }
        size
    }

    /// Convert generalized to bytes
    #[must_use]
    pub fn generalized_to_bytes(&self) -> Vec<u8> {
        self.generalized
            .iter()
            .filter_map(|item| match item {
                GeneralizedItem::Bytes(bytes) => Some(bytes),
                GeneralizedItem::Gap => None,
            })
            .flatten()
            .copied()
            .collect()
    }

    /// Get the generalized input
    #[must_use]
    pub fn generalized(&self) -> &[GeneralizedItem] {
        &self.generalized
    }

    /// Get the generalized input (mutable)
    pub fn generalized_mut(&mut self) -> &mut Vec<GeneralizedItem> {
        &mut self.generalized
    }
}

impl<S> MutatedTransform<BytesInput, S> for GeneralizedInputMetadata
where
    S: HasCorpus,
{
    type Post = Self;

    fn try_transform_from(
        base: &Testcase<BytesInput>,
        _state: &S,
        corpus_idx: CorpusId,
    ) -> Result<Self, Error> {
        base.metadata()
            .get::<GeneralizedInputMetadata>()
            .ok_or_else(|| {
                Error::key_not_found(format!(
                    "Couldn't find the GeneralizedInputMetadata for corpus entry {corpus_idx}",
                ))
            })
            .cloned()
    }

    fn try_transform_into(self, _state: &S) -> Result<(BytesInput, Self::Post), Error> {
        Ok((BytesInput::from(self.generalized_to_bytes()), self))
    }
}

impl<S> MutatedTransformPost<S> for GeneralizedInputMetadata
where
    S: HasCorpus,
{
    fn post_exec(
        self,
        state: &mut S,
        _stage_idx: i32,
        corpus_idx: Option<CorpusId>,
    ) -> Result<(), Error> {
        if let Some(corpus_idx) = corpus_idx {
            let mut testcase = state.corpus().get(corpus_idx)?.borrow_mut();
            testcase.metadata_mut().insert(self);
        }
        Ok(())
    }
}
