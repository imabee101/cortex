use cortex_config::ToolFeature;
use cortex_tool_runtime::Tool;

use super::{cortex_build, opencode};

#[test]
fn tool_features_name_the_tools_they_remove() {
    let cases = [
        (
            ToolFeature::AskUserQuestion,
            vec![cortex_build::AskUserQuestionTool.id()],
        ),
        (ToolFeature::ImageEdit, vec![cortex_build::ImageEditTool.id()]),
        (ToolFeature::ImageGen, vec![cortex_build::ImageGenTool.id()]),
        (ToolFeature::LspTools, vec![cortex_build::LspTool.id()]),
        (
            ToolFeature::VideoGen,
            vec![
                cortex_build::ImageToVideoTool.id(),
                cortex_build::ReferenceToVideoTool.id(),
            ],
        ),
        (ToolFeature::WebFetch, vec![cortex_build::WebFetchTool.id()]),
        (
            ToolFeature::WriteFile,
            vec![opencode::OpenCodeWriteTool.id()],
        ),
    ];
    for (feature, tools) in cases {
        let names: Vec<&str> = tools.iter().map(|id| id.as_str()).collect();
        assert_eq!(names, feature.tool_names(), "{feature:?}");
    }
}
