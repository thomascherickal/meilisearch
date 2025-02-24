use candle_core::Tensor;
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config, DTYPE};
// FIXME: currently we'll be using the hub to retrieve model, in the future we might want to embed it into Meilisearch itself
use hf_hub::api::sync::Api;
use hf_hub::{Repo, RepoType};
use tokenizers::{PaddingParams, Tokenizer};

pub use super::error::{EmbedError, Error, NewEmbedderError};
use super::{DistributionShift, Embedding, Embeddings};

#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    Hash,
    PartialEq,
    Eq,
    serde::Deserialize,
    serde::Serialize,
    deserr::Deserr,
)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
#[deserr(rename_all = camelCase, deny_unknown_fields)]
enum WeightSource {
    #[default]
    Safetensors,
    Pytorch,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct EmbedderOptions {
    pub model: String,
    pub revision: Option<String>,
}

impl EmbedderOptions {
    pub fn new() -> Self {
        Self {
            model: "BAAI/bge-base-en-v1.5".to_string(),
            revision: Some("617ca489d9e86b49b8167676d8220688b99db36e".into()),
        }
    }
}

impl Default for EmbedderOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// Perform embedding of documents and queries
pub struct Embedder {
    model: BertModel,
    tokenizer: Tokenizer,
    options: EmbedderOptions,
    dimensions: usize,
}

impl std::fmt::Debug for Embedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Embedder")
            .field("model", &self.options.model)
            .field("tokenizer", &self.tokenizer)
            .field("options", &self.options)
            .finish()
    }
}

impl Embedder {
    pub fn new(options: EmbedderOptions) -> std::result::Result<Self, NewEmbedderError> {
        let device = candle_core::Device::Cpu;
        let repo = match options.revision.clone() {
            Some(revision) => Repo::with_revision(options.model.clone(), RepoType::Model, revision),
            None => Repo::model(options.model.clone()),
        };
        let (config_filename, tokenizer_filename, weights_filename, weight_source) = {
            let api = Api::new().map_err(NewEmbedderError::new_api_fail)?;
            let api = api.repo(repo);
            let config = api.get("config.json").map_err(NewEmbedderError::api_get)?;
            let tokenizer = api.get("tokenizer.json").map_err(NewEmbedderError::api_get)?;
            let (weights, source) = {
                api.get("pytorch_model.bin")
                    .map(|filename| (filename, WeightSource::Pytorch))
                    .or_else(|_| {
                        api.get("model.safetensors")
                            .map(|filename| (filename, WeightSource::Safetensors))
                    })
                    .map_err(NewEmbedderError::api_get)?
            };
            (config, tokenizer, weights, source)
        };

        let config = std::fs::read_to_string(&config_filename)
            .map_err(|inner| NewEmbedderError::open_config(config_filename.clone(), inner))?;
        let config: Config = serde_json::from_str(&config).map_err(|inner| {
            NewEmbedderError::deserialize_config(config, config_filename, inner)
        })?;
        let mut tokenizer = Tokenizer::from_file(&tokenizer_filename)
            .map_err(|inner| NewEmbedderError::open_tokenizer(tokenizer_filename, inner))?;

        let vb = match weight_source {
            WeightSource::Pytorch => VarBuilder::from_pth(&weights_filename, DTYPE, &device)
                .map_err(NewEmbedderError::pytorch_weight)?,
            WeightSource::Safetensors => unsafe {
                VarBuilder::from_mmaped_safetensors(&[weights_filename], DTYPE, &device)
                    .map_err(NewEmbedderError::safetensor_weight)?
            },
        };

        let model = BertModel::load(vb, &config).map_err(NewEmbedderError::load_model)?;

        if let Some(pp) = tokenizer.get_padding_mut() {
            pp.strategy = tokenizers::PaddingStrategy::BatchLongest
        } else {
            let pp = PaddingParams {
                strategy: tokenizers::PaddingStrategy::BatchLongest,
                ..Default::default()
            };
            tokenizer.with_padding(Some(pp));
        }

        let mut this = Self { model, tokenizer, options, dimensions: 0 };

        let embeddings = this
            .embed(vec!["test".into()])
            .map_err(NewEmbedderError::hf_could_not_determine_dimension)?;
        this.dimensions = embeddings.first().unwrap().dimension();

        Ok(this)
    }

    pub fn embed(
        &self,
        mut texts: Vec<String>,
    ) -> std::result::Result<Vec<Embeddings<f32>>, EmbedError> {
        let tokens = match texts.len() {
            1 => vec![self
                .tokenizer
                .encode(texts.pop().unwrap(), true)
                .map_err(EmbedError::tokenize)?],
            _ => self.tokenizer.encode_batch(texts, true).map_err(EmbedError::tokenize)?,
        };
        let token_ids = tokens
            .iter()
            .map(|tokens| {
                let tokens = tokens.get_ids().to_vec();
                Tensor::new(tokens.as_slice(), &self.model.device).map_err(EmbedError::tensor_shape)
            })
            .collect::<Result<Vec<_>, EmbedError>>()?;

        let token_ids = Tensor::stack(&token_ids, 0).map_err(EmbedError::tensor_shape)?;
        let token_type_ids = token_ids.zeros_like().map_err(EmbedError::tensor_shape)?;
        let embeddings =
            self.model.forward(&token_ids, &token_type_ids).map_err(EmbedError::model_forward)?;

        // Apply some avg-pooling by taking the mean embedding value for all tokens (including padding)
        let (_n_sentence, n_tokens, _hidden_size) =
            embeddings.dims3().map_err(EmbedError::tensor_shape)?;

        let embeddings = (embeddings.sum(1).map_err(EmbedError::tensor_value)? / (n_tokens as f64))
            .map_err(EmbedError::tensor_shape)?;

        let embeddings: Vec<Embedding> = embeddings.to_vec2().map_err(EmbedError::tensor_shape)?;
        Ok(embeddings.into_iter().map(Embeddings::from_single_embedding).collect())
    }

    pub fn embed_chunks(
        &self,
        text_chunks: Vec<Vec<String>>,
    ) -> std::result::Result<Vec<Vec<Embeddings<f32>>>, EmbedError> {
        text_chunks.into_iter().map(|prompts| self.embed(prompts)).collect()
    }

    pub fn chunk_count_hint(&self) -> usize {
        1
    }

    pub fn prompt_count_in_chunk_hint(&self) -> usize {
        std::thread::available_parallelism().map(|x| x.get()).unwrap_or(8)
    }

    pub fn dimensions(&self) -> usize {
        self.dimensions
    }

    pub fn distribution(&self) -> Option<DistributionShift> {
        if self.options.model == "BAAI/bge-base-en-v1.5" {
            Some(DistributionShift { current_mean: 0.85, current_sigma: 0.1 })
        } else {
            None
        }
    }
}
