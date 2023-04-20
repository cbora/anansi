use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::bail;
use ndarray::{s, Array, ArrayView, Dim};
use ort::{
    tensor::DynOrtTensor, tensor::FromArray, tensor::InputTensor, tensor::OrtOwnedTensor,
    GraphOptimizationLevel, Session, SessionBuilder,
};
use phf::phf_map;
use tokenizers::tokenizer::Tokenizer;
use tracing::info;

use crate::embedder;
use crate::embedder::Embedder;
use crate::image_processor::ImageProcessor;
use crate::utils::download_model_sync;

pub struct CLIPEmbedder {
    #[allow(dead_code)]
    session_visual: Session,
    session_textual: Session,
    tokenizer: Tokenizer,
    image_processor: Arc<ImageProcessor>,
    model_name: String,
}

#[derive(Clone, Copy)]
struct CLIPModel {
    pub textual: &'static str,
    pub textual_hash: &'static str,
    pub visual: &'static str,
    pub visual_hash: &'static str,
    pub image_size: i32,
}
static S3_BUCKET_V2: &'static str = "https://clip-as-service.s3.us-east-2.amazonaws.com/models-436c69702d61732d53657276696365/onnx/";
static CLIP_MODELS: phf::Map<&'static str, CLIPModel> = phf_map! {
    "CLIP_RN50_OPENAI" => CLIPModel{
        textual: "RN50/textual.onnx",
        textual_hash:"722418bfe47a1f5c79d1f44884bb3103",
        visual: "RN50/visual.onnx",
        visual_hash: "5761475db01c3abb68a5a805662dcd10",
        image_size: 224,
    },
    "CLIP_RN50_YFCC15M" => CLIPModel{
        textual: "RN50-yfcc15m/textual.onnx",
        textual_hash: "4ff2ea7228b9d2337b5440d1955c2108",
        visual: "RN50-yfcc15m/visual.onnx",
        visual_hash: "87daa9b4a67449b5390a9a73b8c15772",
        image_size: 224,
    },
    "CLIP_RN50_CC12M" => CLIPModel{
        textual: "RN50-cc12m/textual.onnx",
        textual_hash: "78fa0ae0ea47aca4b8864f709c48dcec",
        visual: "RN50-cc12m/visual.onnx",
        visual_hash: "0e04bf92f3c181deea2944e322ebee77",
        image_size: 224,
    },
    "CLIP_VIT_L_14_336_OPENAI" => CLIPModel {
        textual: "ViT-L-14@336px/textual.onnx",
        textual_hash: "78fab479f136403eed0db46f3e9e7ed2",
        visual: "ViT-L-14@336px/visual.onnx",
        visual_hash: "f3b1f5d55ca08d43d749e11f7e4ba27e",
        image_size: 336
    },
    // "CLIP_"
};

// static CLIP_MODEL_VISUAL_SIZING: phf::Ma<&'static str, i32> = phf_map! {
//     "CLIP_RN50_OPENAI": 224,
//     "CLIP_RN101_OPENAI": 224,
//     "CLIP_RN50X4_OPENAI": 288,
//     "CLIP_RN50X16_OPENAI": 384,
//     "CLIP_RN50X64_OPENAI": 448,
//     "CLIP_VIT_B_32": 224,
//     "CLIP_ROBERTA_VIT_B_32": 234,
//     "CLIP_XLM_ROBERTA_BASE_VIT_B_32": 224,
//     "CLIP_VIT_B_16": 224,
//     "CLIP_VIT_B_16_PLUS": 240,
//     "CLIP_VIT_B_16_PLUS_240": 240,
//     "CLIP_VIT_L_14": 224,
//     "CLIP_VIT_L_14_336_OPENAI": 336,
//     "CLIP_VIT_H_14": 224,
//     "CLIP_XLM_ROBERTA_LARGE_VIT_H_14": 224,
//     "CLIP_VIT_G_14": 224,

// };

impl Embedder for CLIPEmbedder {
    fn new(params: &embedder::EmbedderParams) -> anyhow::Result<Self> {
        CLIPEmbedder::new(params)
    }
    fn encode(&self, req: &embedder::EmebeddingRequest) -> anyhow::Result<Vec<Vec<f32>>> {
        let req_clip: &embedder::CLIPParams = match req {
            embedder::EmebeddingRequest::CLIPRequest { params } => &params,
            _ => {
                unreachable!("incorrect params passed for construction")
            }
        };
        match req_clip {
            embedder::CLIPParams::Text { vals } => {
                return self.encode_text_batch(vals);
            }
            embedder::CLIPParams::Uri { vals } => {
                return self.encode_image_batch(vals);
            }
            embedder::CLIPParams::UriBytes { vals: _ } => {
                bail!("this is not implemented as yet...")
            }
        }
    }
}
impl CLIPEmbedder {
    fn gen_text_encodings(
        &self,
        idx: usize,
        context_length: usize,
        text: &str,
    ) -> anyhow::Result<(Array<i32, Dim<[usize; 2]>>, Array<i32, Dim<[usize; 2]>>)> {
        let text_encoding;
        match self.tokenizer.encode(text, true) {
            Ok(res) => text_encoding = res,
            Err(err) => {
                bail!(
                    "unable to tokenize the input at idx: {} | err: {}",
                    idx,
                    err
                );
            }
        }
        let mut i_tvals: Vec<i32> = text_encoding
            .get_ids()
            .iter()
            .cloned()
            .map(|x| x as i32)
            .collect();
        let i_tvals_diff = context_length - i_tvals.len();
        if i_tvals_diff > 0 {
            i_tvals.extend(std::iter::repeat(0).take(i_tvals_diff));
        }
        let a: Array<i32, _> = ArrayView::from_shape((1, i_tvals.len()), &i_tvals)?.to_owned();
        let mut i_atten_mask: Vec<i32> = text_encoding
            .get_attention_mask()
            .iter()
            .cloned()
            .map(|x| x as i32)
            .collect();
        let i_atten_mask_diff = context_length - i_atten_mask.len();
        if i_atten_mask_diff > 0 {
            i_atten_mask.extend(std::iter::repeat(0).take(i_atten_mask_diff));
        }
        let b: Array<i32, _> =
            ArrayView::from_shape((1, i_atten_mask.len()), &i_atten_mask)?.to_owned();
        Ok((a, b))
    }
    pub fn encode_text_batch(&self, text_arr: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        // let mut all_a: Vec<Array<i32, Dim<[usize; 2]>
        let context_length: usize = 77;
        let mut a = Array::<i32, _>::zeros((text_arr.len(), context_length));
        let mut b = Array::<i32, _>::zeros((text_arr.len(), context_length));
        for idx in 0..text_arr.len() {
            let (a_partial, b_partial) =
                self.gen_text_encodings(idx, context_length, &text_arr[idx])?;
            a.slice_mut(s![idx..idx + 1, 0..context_length])
                .assign(&a_partial);
            b.slice_mut(s![idx..idx + 1, 0..context_length])
                .assign(&b_partial);
        }
        let outputs: Vec<DynOrtTensor<ndarray::Dim<ndarray::IxDynImpl>>> =
            self.session_textual.run([
                InputTensor::from_array(a.into_dyn()),
                InputTensor::from_array(b.into_dyn()),
            ])?;

        let embedding: OrtOwnedTensor<f32, _> = outputs[outputs.len() - 1].try_extract().unwrap();
        let embedding = embedding.view().to_owned();
        let mut result: Vec<Vec<f32>> = Vec::with_capacity(text_arr.len());
        for idx in 0..text_arr.len() {
            result.push(
                embedding
                    .slice(s![idx..idx + 1, ..])
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>(),
            );
        }
        Ok(result)
    }

    pub fn encode_image_batch(&self, uri_arr: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        let img_dims;
        match CLIP_MODELS.get(&self.model_name) {
            Some(m) => img_dims = m.image_size,
            None => {
                bail!(
                    "CLIP model: {} was not found - this should never happen?",
                    self.model_name
                );
            }
        };

        let uri_vecs: anyhow::Result<Vec<Array<f32, Dim<[usize; 3]>>>> = uri_arr
            .iter()
            .map(|uri| self.image_processor.uri_to_clip_vec(uri, img_dims))
            .collect();
        // expected format:  num_uris, 3, onnx_model.image_size, onnx_model.image_size
        let md_vecs = uri_vecs?;
        let mut a = Array::<f32, _>::zeros((
            md_vecs.len() as usize,
            3 as usize,
            img_dims as usize,
            img_dims as usize,
        ));
        md_vecs.iter().enumerate().for_each(|(idx, md_vec)| {
            a.slice_mut(s![idx..idx + 1, 0..3, 0..img_dims, 0..img_dims])
                .assign(&md_vec);
        });

        let outputs: Vec<DynOrtTensor<ndarray::Dim<ndarray::IxDynImpl>>> = self
            .session_visual
            .run([InputTensor::from_array(a.into_dyn())])?;
        let embedding: OrtOwnedTensor<f32, _> = outputs[outputs.len() - 1].try_extract().unwrap();
        let embedding = embedding.view().to_owned();
        let mut result: Vec<Vec<f32>> = Vec::with_capacity(md_vecs.len());
        for idx in 0..md_vecs.len() {
            result.push(
                embedding
                    .slice(s![idx..idx + 1, ..])
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>(),
            );
        }
        Ok(result)
    }

    pub fn encode(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        // we need to handle image resizing + handling!
        let text_encoding;
        match self.tokenizer.encode(text, true) {
            Ok(res) => text_encoding = res,
            Err(err) => {
                bail!("unable to tokenize the input: {}", err)
            }
        }
        // TODO(infrawhispers) context_length is expected to be 77 by the ONNX models served by JinaAI here:
        // https://github.com/jina-ai/clip-as-service/blob/1888ef65f20a94b38f318696e663d447c7cb1dc6/server/clip_server/model/tokenization.py
        // if we end up allowing for the chinese + multilanguage models, this code will need to change.
        let context_length: usize = 77;
        let mut i_tvals: Vec<i32> = text_encoding
            .get_ids()
            .iter()
            .cloned()
            .map(|x| x as i32)
            .collect();
        let i_tvals_diff = context_length - i_tvals.len();
        if i_tvals_diff > 0 {
            i_tvals.extend(std::iter::repeat(0).take(i_tvals_diff));
        }
        let a: Array<i32, _> = ArrayView::from_shape((1, i_tvals.len()), &i_tvals)?.to_owned();

        let mut i_atten_mask: Vec<i32> = text_encoding
            .get_attention_mask()
            .iter()
            .cloned()
            .map(|x| x as i32)
            .collect();
        let i_atten_mask_diff = context_length - i_atten_mask.len();
        if i_atten_mask_diff > 0 {
            i_atten_mask.extend(std::iter::repeat(0).take(i_atten_mask_diff));
        }

        let b: Array<i32, _> =
            ArrayView::from_shape((1, i_atten_mask.len()), &i_atten_mask)?.to_owned();
        let outputs: Vec<DynOrtTensor<ndarray::Dim<ndarray::IxDynImpl>>> =
            self.session_textual.run([
                InputTensor::from_array(a.into_dyn()),
                InputTensor::from_array(b.into_dyn()),
            ])?;

        let embedding: OrtOwnedTensor<f32, _> = outputs[outputs.len() - 1].try_extract().unwrap();
        let embedding = embedding.view();
        let result: Vec<f32> = embedding.to_owned().iter().cloned().collect::<Vec<_>>();
        Ok(result)
    }
    pub fn new(params: &embedder::EmbedderParams) -> anyhow::Result<Self> {
        let model_details: CLIPModel;
        match CLIP_MODELS.get(params.model_name) {
            Some(m) => model_details = *m,
            None => {
                bail!(
                    "CLIP model: {} was not found; below is a list of all available models...",
                    params.model_name
                );
            }
        };
        if !params.model_path.exists() {
            fs::create_dir_all(params.model_path)?;
        }
        let text_model_fp = PathBuf::from(params.model_path).join("textual.onnx");
        if !text_model_fp.exists() {
            info!(
                model = params.model_name,
                "textual.onnx does not exist, downloading from remote"
            );
            download_model_sync(
                &format!("{}.{}", params.model_name, "textual.onnx"),
                &format!("{}{}", S3_BUCKET_V2, model_details.textual),
                true,
                &text_model_fp,
                model_details.textual_hash,
            )?;
        }
        let visual_model_fp = PathBuf::from(params.model_path).join("visual.onnx");
        if !visual_model_fp.exists() {
            info!(
                model = params.model_name,
                "visual.onnx does not exist, downloading from remote"
            );
            download_model_sync(
                &format!("{}.{}", params.model_name, "visual.onnx"),
                &format!("{}{}", S3_BUCKET_V2, model_details.visual),
                true,
                &visual_model_fp,
                model_details.visual_hash,
            )?;
        }
        info!(model = params.model_name, "building the session_textual");
        let session_textual = SessionBuilder::new(&params.ort_environment)?
            .with_optimization_level(GraphOptimizationLevel::Level1)?
            .with_inter_threads(params.num_threads)?
            .with_parallel_execution(true)?
            .with_model_from_file(text_model_fp)?;
        info!(model = params.model_name, "building the session_visual");
        let session_visual = SessionBuilder::new(&params.ort_environment)?
            .with_optimization_level(GraphOptimizationLevel::Level1)?
            .with_inter_threads(params.num_threads)?
            .with_parallel_execution(true)?
            .with_model_from_file(visual_model_fp)?;
        info!(model = params.model_name, "building the tokenizer");
        let tokenizer: Tokenizer;
        match Tokenizer::from_pretrained("openai/clip-vit-base-patch16", None) {
            Ok(tk) => tokenizer = tk,
            Err(err) => {
                bail!("unable to create a tokenizer: {}", err);
            }
        }
        Ok(CLIPEmbedder {
            session_textual: session_textual,
            session_visual: session_visual,
            tokenizer: tokenizer,
            image_processor: params.img_processor.clone(),
            model_name: params.model_name.to_string(),
        })
    }
}
