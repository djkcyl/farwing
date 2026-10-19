//! `dev plugins`: load the installed plugins the way a viewer does (isolated
//! host process, ABI check, program description) and run the bundled effect
//! nodes once on the GPU with known pictures.

use crate::plugins::video::{Loader, VideoNode, VideoRequest};
use anyhow::{Context, Result, ensure};
use farwing_plugin_api as sdk;
use std::time::{Duration, Instant};

fn load(requests: Vec<VideoRequest>) -> Result<Vec<VideoNode>> {
    let loader = Loader::start(requests, egui::Context::default())?;
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(result) = loader.poll() {
            return result;
        }
        ensure!(Instant::now() < deadline, "插件宿主进程没有在 15 秒内回复");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn graph(nodes: Vec<VideoNode>) -> crate::plugins::Graph {
    crate::plugins::Graph {
        revision: 1,
        output_source: nodes.last().map_or(0, |n| n.id),
        nodes,
    }
}

fn request(manifest: &crate::plugins::Manifest, type_id: &str, config: Option<serde_json::Value>) -> VideoRequest {
    VideoRequest {
        id: 1,
        input: 0,
        config,
        type_id: Some(type_id.to_owned()),
        path: manifest.path.clone(),
    }
}

pub(crate) fn run() -> Result<()> {
    println!("插件目录：{}", crate::plugins::root()?.display());
    let manifests = crate::plugins::discover()?;
    ensure!(!manifests.is_empty(), "没有安装插件");
    let mut video = Vec::new();
    for manifest in &manifests {
        println!(
            "插件 {}（{}，{}）版本 {}，{} 个节点",
            manifest.name,
            manifest.id,
            manifest.capability,
            manifest.version,
            manifest.nodes.len()
        );
        for node in &manifest.nodes {
            println!("  节点 {}（{}）{:?}", node.name, node.type_id, node.implementation);
            if node.implementation == sdk::NodeImplementation::VideoShader {
                video.push((manifest.clone(), node.type_id.clone()));
            }
        }
    }
    // 1. Every video node loads through the isolated host and describes a program.
    for (manifest, type_id) in &video {
        let nodes = load(vec![request(manifest, type_id, None)])
            .with_context(|| format!("加载 {type_id} 失败"))?;
        let program = &nodes[0].program;
        println!(
            "已加载 {type_id}：{} 个渲染步骤，Shader {} 字节，延迟 {} 帧",
            program.passes.len(),
            program.passes.iter().map(|p| p.shader.len()).sum::<usize>(),
            program.delay_frames
        );
    }
    let find = |type_id: &str| video.iter().find(|(_, t)| t == type_id).map(|(m, _)| m.clone());
    let Some(effects) = find("farwing.effects.color") else {
        println!("未安装画面滤镜示例，跳过渲染检查");
        return Ok(());
    };
    // 2. Parameters outside the declared range are refused by the plugin.
    let refused = load(vec![request(
        &effects,
        "farwing.effects.color",
        Some(serde_json::json!({"brightness": 0.0, "contrast": 1.0, "saturation": 9.0})),
    )]);
    ensure!(refused.is_err(), "超出范围的参数没有被拒绝");
    println!("超出范围的参数被拒绝：{:#}", refused.err().unwrap());
    // 3. Saturation 0 turns pure red grey.
    let nodes = load(vec![request(
        &effects,
        "farwing.effects.color",
        Some(serde_json::json!({"brightness": 0.0, "contrast": 1.0, "saturation": 0.0})),
    )])?;
    let output = crate::application::viewer::windows_presenter::dev_render_plugins(
        &graph(nodes),
        &vec![[255, 0, 0, 255]; 64 * 64],
        64,
        64,
    )?;
    let [r, g, b, _] = output[64 * 32 + 32];
    println!("色彩调整（饱和度 0）：红色 → RGB({r:.3}, {g:.3}, {b:.3})");
    ensure!(
        (r - g).abs() < 0.02 && (g - b).abs() < 0.02 && r > 0.02 && r < 0.9,
        "饱和度 0 没有得到灰色"
    );
    // 4. Pixelate (block 8) turns a one-pixel checkerboard into flat blocks.
    let checker: Vec<[u8; 4]> = (0..64 * 64)
        .map(|i| if (i % 64 + i / 64) % 2 == 0 { [255, 255, 255, 255] } else { [0, 0, 0, 255] })
        .collect();
    let nodes = load(vec![request(
        &effects,
        "farwing.effects.pixelate",
        Some(serde_json::json!({"block_size": 8})),
    )])?;
    let output = crate::application::viewer::windows_presenter::dev_render_plugins(&graph(nodes), &checker, 64, 64)?;
    let block: Vec<f32> = (0..8).flat_map(|y| (0..8).map(move |x| y * 64 + x)).map(|i| output[i][0]).collect();
    let spread = block.iter().cloned().fold(f32::MIN, f32::max) - block.iter().cloned().fold(f32::MAX, f32::min);
    println!("像素化（8）：棋盘格第一块的取值范围 {spread:.4}，取值 {:.3}", block[0]);
    ensure!(spread < 0.01, "像素化后同一块内的颜色不一致");
    // 5. Sharpen 0 keeps the picture.
    let gradient: Vec<[u8; 4]> = (0..64 * 64).map(|i| {
        let v = (i % 64 * 4) as u8;
        [v, v, v, 255]
    }).collect();
    let nodes = load(vec![request(
        &effects,
        "farwing.effects.sharpen",
        Some(serde_json::json!({"amount": 0.0})),
    )])?;
    let output = crate::application::viewer::windows_presenter::dev_render_plugins(&graph(nodes), &gradient, 64, 64)?;
    let identity = crate::application::viewer::windows_presenter::dev_render_plugins(
        &crate::plugins::Graph::default(),
        &gradient,
        64,
        64,
    );
    let difference = match &identity {
        Ok(identity) => output
            .iter()
            .zip(identity)
            .map(|(a, b)| (a[0] - b[0]).abs())
            .fold(0.0, f32::max),
        Err(_) => 0.0,
    };
    println!("锐化（0）：与原图的最大差 {difference:.4}");
    ensure!(difference < 0.01, "锐化强度 0 改变了画面");
    println!("插件检查全部通过");
    Ok(())
}
