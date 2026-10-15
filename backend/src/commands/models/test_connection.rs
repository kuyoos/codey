use super::*;

/// 零成本探测一条线路和某个模型是否可用。
///
/// 这里只复用只读的「拉取线路模型列表」能力，不发送任何对话/messages 请求：
/// 真实请求会消耗上游额度并产生计费，而 `/models` 一类接口是只读的，能在不
/// 产生费用、不写入配置的前提下说明凭据与端点是否可用。因此结果分成两件事：
/// 连接是否成功，以及返回的模型列表里有没有目标模型（模型 ID 只是字符串，
/// 凭据在所属线路上）。
pub(crate) async fn test_route_model(
    state: &Arc<AppState>,
    route_id: String,
    model: String,
) -> Result<Value, String> {
    let config = state.config.read().await.clone();
    let route_id = route_id.trim();
    let profile = config
        .profiles
        .iter()
        .find(|profile| profile.id == route_id)
        .cloned()
        .ok_or_else(|| "找不到要测试的线路".to_string())?;
    profile.validate()?;
    // 原生传输插件的模型由插件描述声明，上游不保证提供标准 /models。
    if profile.plugin_owner_id.is_some()
        && profile
            .plugin_route_spec
            .as_ref()
            .is_some_and(|spec| spec.transport.is_some())
    {
        return Err("这条线路的模型由原生插件声明，无法在线探测".to_string());
    }
    let models = if profile.official_account {
        fetch_official_route_models(state, &profile)
            .await?
            .iter()
            .filter_map(|model| {
                model
                    .get("slug")
                    .or_else(|| model.get("id"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .collect::<Vec<_>>()
    } else {
        fetch_provider_models(profile.clone())
            .await
            .map_err(|error| error.to_string())?
    };
    let found = models
        .iter()
        .any(|candidate| model_id::equal(candidate, &model));
    Ok(json!({
        "routeId": route_id,
        "model": model,
        "modelCount": models.len(),
        "found": found,
    }))
}
