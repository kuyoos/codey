// Codex 渲染进程资产源补丁。原本是主进程 protocol.handle 里对 app:// 响应
// 做改写的纯函数部分；Electron fuse 关闭后主进程注入不可用，改由 CDP Fetch 会话
// 在响应暂停时逐资产调用 globalThis.__codeyRendererPatchEntry 完成同样的改写。
// 本文件只依赖页面 JS 环境，不使用任何 Node/Electron API。
(() => {
  const rendererPatchOptions =
    typeof globalThis.__CODEY_RENDERER_PATCH_OPTIONS__ === "object" &&
    globalThis.__CODEY_RENDERER_PATCH_OPTIONS__ !== null
      ? globalThis.__CODEY_RENDERER_PATCH_OPTIONS__
      : {};
  const disablePet = rendererPatchOptions.disablePet === true;
  const threadOwnerDiscoveryTimeoutMs = 150;
  // 渲染进程没有 --codey-record-error 子进程可写：补丁失败经 CDP
  // Runtime.addBinding 暴露的 __codeyPatchLog 回传给 Rust 记录。
  const recordCodeyPatchFailure = (operation, error, context = {}) => {
    try {
      const message = error instanceof Error
        ? `${error.name}: ${error.message}${error.stack ? `\n${error.stack}` : ""}`
        : String(error || "unknown patch failure");
      const record = JSON.stringify({ operation, message, context });
      const reporter = globalThis.__codeyPatchLog;
      if (typeof reporter === "function") {
        reporter(record);
      } else {
        console.error("[Codey]", record);
      }
    } catch {}
  };
  const maxRendererPatchFingerprints = 64;
  const rendererPatchFailuresByFingerprint = new Map();
  let activeRendererPatchFailures = null;
  // 指纹只作补丁结果缓存键，不需要加密强度；原主进程版用 sha256，
  // 页面里用同步 FNV-1a 变体即可，省掉 async crypto.subtle。
  const rendererPatchFingerprint = (source) => {
    try {
      let hash = 0xcbf29ce484222325n;
      for (let index = 0; index < source.length; index += 1) {
        hash ^= BigInt(source.charCodeAt(index));
        hash = BigInt.asUintN(64, hash * 0x100000001b3n);
      }
      return `${source.length}:${hash.toString(36)}`;
    } catch {
      // Fingerprinting is only an optimization. If it is unavailable, keep
      // the existing compatibility behavior instead of risking a false cache hit.
      return null;
    }
  };
  // Patching is a pure function of the asset text: reloads, renderer recovery
  // and the locale-forced first-launch reload re-request the same app-initial
  // chunk, so remember the patched output instead of rerunning every gate.
  const maxRendererPatchedOutputs = 4;
  const rendererPatchedOutputByFingerprint = new Map();
  const rememberRendererPatchedOutput = (fingerprint, patched) => {
    if (fingerprint == null) return;
    rendererPatchedOutputByFingerprint.delete(fingerprint);
    rendererPatchedOutputByFingerprint.set(fingerprint, patched);
    while (rendererPatchedOutputByFingerprint.size > maxRendererPatchedOutputs) {
      const oldest = rendererPatchedOutputByFingerprint.keys().next().value;
      rendererPatchedOutputByFingerprint.delete(oldest);
    }
  };
  const rendererPatchFailuresForFingerprint = (fingerprint) => {
    if (fingerprint == null) return null;
    const existing = rendererPatchFailuresByFingerprint.get(fingerprint);
    if (existing) {
      // Refresh insertion order so the bounded map behaves as an LRU.
      rendererPatchFailuresByFingerprint.delete(fingerprint);
      rendererPatchFailuresByFingerprint.set(fingerprint, existing);
      return existing;
    }
    const failures = new Set();
    rendererPatchFailuresByFingerprint.set(fingerprint, failures);
    while (rendererPatchFailuresByFingerprint.size > maxRendererPatchFingerprints) {
      const oldest = rendererPatchFailuresByFingerprint.keys().next().value;
      rendererPatchFailuresByFingerprint.delete(oldest);
    }
    return failures;
  };
  // Each renderer gate is optional and independent. Codex bundles are minified
  // and reshape between releases, so a single drifted anchor must skip only its
  // own gate — never discard the sibling gates that are still compatible. That
  // is what previously hid the whole Fast/service-tier control on the builds
  // where one unrelated anchor moved: an exception here aborted every gate on
  // the asset. Log and return the source unchanged so the rest still apply.
  // Field builds ship minified bundles whose shapes drift between platforms
  // and releases. When a gate matches nothing, these are the neighborhood
  // markers every gate sits near; capturing printable windows around them lets
  // a field diagnostic log be turned into the next compatible variant without
  // reproducing that exact bundle locally.
  const rendererGateDiagnosticAnchors = [
    "`composer.toggleFastMode`",
    "composer.speedSlashCommand.disableDescription",
    "isServiceTierAllowed",
    "selectedServiceTier",
    "featureRequirements?.fast_mode",
    "useHiddenModels",
    "availableOptions.length",
    "includeUltraReasoningEffort",
    "isCustomModelProvider",
  ];
  const rendererGateFailureExcerpts = (source) => {
    const excerpts = [];
    for (const anchor of rendererGateDiagnosticAnchors) {
      if (excerpts.length >= 2) break;
      const index = source.indexOf(anchor);
      if (index < 0) continue;
      excerpts.push(
        source
          .slice(Math.max(0, index - 150), index + anchor.length + 190)
          .replace(/[^\x20-\x7E]/g, "?"),
      );
    }
    return excerpts;
  };
  const recordIncompatibleRendererGate = (source, name, matchCount) => {
    activeRendererPatchFailures?.add(name);
    const message =
      `Codey skipped an incompatible Codex renderer patch: ${name} gate matched ${matchCount} times`;
    const context = { matchCount };
    const excerpts = rendererGateFailureExcerpts(source);
    if (excerpts.length) context.excerpts = excerpts;
    recordCodeyPatchFailure(`renderer_patch:${name}`, message, context);
    try {
      console.error(message);
    } catch {}
    return source;
  };
  const replaceUniqueRendererGate = (source, pattern, replacement, name) => {
    // app:// assets can be requested repeatedly during reloads or renderer
    // recovery. A gate already known to be incompatible with the exact same
    // source must remain skipped without rerunning its full-bundle regexes or
    // spawning another error-log helper.
    if (activeRendererPatchFailures?.has(name)) return source;
    const gates = Array.isArray(pattern) ? pattern : [{ pattern, replacement }];
    let matchCount = 0;
    let patched = source;
    for (const gate of gates) {
      let gateCount = 0;
      const candidate = source.replace(gate.pattern, (...args) => {
        gateCount += 1;
        return typeof gate.replacement === "function"
          ? gate.replacement(...args)
          : gate.replacement;
      });
      if (gateCount > 0 && matchCount === 0) patched = candidate;
      matchCount += gateCount;
    }
    if (matchCount !== 1) {
      return recordIncompatibleRendererGate(source, name, matchCount);
    }
    return patched;
  };
  const replaceNearestRendererGateBeforeAnchor = (
    source,
    pattern,
    replacement,
    name,
    anchor,
    maximumDistance,
  ) => {
    if (activeRendererPatchFailures?.has(name)) return source;
    const anchorIndexes = [];
    for (
      let index = source.indexOf(anchor);
      index >= 0;
      index = source.indexOf(anchor, index + anchor.length)
    ) anchorIndexes.push(index);
    if (anchorIndexes.length !== 1) {
      return recordIncompatibleRendererGate(source, name, anchorIndexes.length);
    }

    const anchorIndex = anchorIndexes[0];
    const scopeStart = Math.max(0, anchorIndex - maximumDistance);
    const scope = source.slice(scopeStart, anchorIndex + anchor.length);
    const gates = Array.isArray(pattern) ? pattern : [{ pattern, replacement }];
    const candidates = [];
    for (const gate of gates) {
      scope.replace(gate.pattern, (...args) => {
        candidates.push({ args, gate, offset: args.at(-2) });
        return args[0];
      });
    }
    if (candidates.length === 0) {
      return recordIncompatibleRendererGate(source, name, 0);
    }

    const nearestOffset = Math.max(
      ...candidates.map((candidate) => candidate.offset),
    );
    const nearestCandidates = candidates.filter(
      (candidate) => candidate.offset === nearestOffset,
    );
    if (nearestCandidates.length !== 1) {
      return recordIncompatibleRendererGate(
        source,
        name,
        nearestCandidates.length,
      );
    }

    const [{ args, gate, offset }] = nearestCandidates;
    const effectiveReplacement = gate.replacement ?? replacement;
    const replaced = typeof effectiveReplacement === "function"
      ? effectiveReplacement(...args)
      : effectiveReplacement;
    const absoluteOffset = scopeStart + offset;
    return source.slice(0, absoluteOffset) +
      replaced +
      source.slice(absoluteOffset + args[0].length);
  };
  const rendererHasNativeCustomProviderModelAccess = (source) =>
    /function\s+[$A-Z_a-z][$\w]*\(\{[^}]*isCustomModelProvider\s*:\s*([$A-Z_a-z][$\w]*)[^}]*model\s*:\s*([$A-Z_a-z][$\w]*)[^}]*useHiddenModels\s*:\s*([$A-Z_a-z][$\w]*)[^}]*\}\)\s*\{\s*return[\s\S]{0,512}?\3\s*&&\s*!\s*\1\s*&&[\s\S]{0,256}?\?\s*[$A-Z_a-z][$\w]*\.has\(\s*\2\.model\s*\)\s*:\s*!\s*\2\.hidden\s*\)*\s*\}/.test(
      source,
    );
  const replacePetRendererImportWithStubs = (match, importClause) => {
    if (typeof importClause !== "string" || importClause.trim() === "") {
      return "";
    }
    const localBindings = [];
    const rememberBinding = (binding) => {
      if (
        /^[$A-Z_a-z][$\w]*$/.test(binding)
        && !localBindings.includes(binding)
      ) {
        localBindings.push(binding);
      }
    };
    const defaultBinding = importClause.match(/^\s*([$A-Z_a-z][$\w]*)/);
    if (defaultBinding) rememberBinding(defaultBinding[1]);
    for (const specifier of importClause.matchAll(
      /(?:^|[,{])\s*([$A-Z_a-z][$\w]*)(?:\s+as\s+([$A-Z_a-z][$\w]*))?\s*(?=[,}])/g,
    )) {
      rememberBinding(specifier[2] ?? specifier[1]);
    }
    for (const namespace of importClause.matchAll(
      /\*\s+as\s+([$A-Z_a-z][$\w]*)/g,
    )) {
      rememberBinding(namespace[1]);
    }
    if (!localBindings.length) {
      const message =
        "Codey could not identify Codex pet settings renderer import bindings";
      recordCodeyPatchFailure("renderer_patch:pet settings avatar resources", message);
      try {
        console.error(message);
      } catch {}
      return match;
    }
    const [firstBinding, ...aliases] = localBindings;
    const aliasDeclarations = aliases
      .map((binding) => `,${binding}=${firstBinding}`)
      .join("");
    return `const ${firstBinding}=(()=>{const target=function(){return null};return new Proxy(target,{get(target,property,receiver){if(property===Symbol.iterator)return function*(){};if(property===\`map\`||property===\`filter\`||property===\`flatMap\`||property===\`slice\`)return()=>[];if(property===\`then\`)return void 0;return Reflect.get(target,property,receiver)},construct(){return{}}})})()${aliasDeclarations};`;
  };
  // Older Codex builds imported a single codex-avatar chunk from the settings
  // chunk; current builds split the preview across the mascot button, the pet
  // asset table, and the avatar option hook. One settings chunk can import
  // several of them, so every match is stubbed rather than requiring a unique
  // one.
  const petRendererResourceImport =
    /import(?:\s*([^;"']+?)\s*from)?\s*["']\.\/(?:codex-avatar|codex-pet-assets|avatar-mascot-button|use-avatar-options)(?:[~-][^/"']*)?\.js["']/;
  const petRendererResourceImportAll = new RegExp(
    petRendererResourceImport.source,
    "g",
  );
  const replacePetRendererResourceImports = (source, name) => {
    if (activeRendererPatchFailures?.has(name)) return source;
    let count = 0;
    const patched = source.replace(petRendererResourceImportAll, (...args) => {
      count += 1;
      return replacePetRendererImportWithStubs(...args);
    });
    if (count === 0) return recordIncompatibleRendererGate(source, name, count);
    return patched;
  };
  const threadOwnerDiscoveryExpression = (
    coordinationName,
    hostIdName,
    conversationIdName,
  ) =>
    [
      "await (globalThis.__CODEY_THREAD_OWNER_DISCOVERY_V2__??=(()=>{",
      "const requestsByClient=new WeakMap;",
      "return{find(client,hostId,conversationId){",
      "let requests=requestsByClient.get(client);",
      "if(requests==null){requests=new Map;requestsByClient.set(client,requests)}",
      "const key=String(hostId)+String.fromCharCode(0)+String(conversationId);",
      "const existing=requests.get(key);",
      "if(existing!=null)return existing;",
      "let settled=false,timer;",
      "const lookup=Promise.resolve().then(()=>client.findThreadOwner({hostId,conversationId}));",
      "const request=new Promise((resolve,reject)=>{",
      `timer=globalThis.setTimeout(()=>{if(settled)return;settled=true;resolve(null)},${threadOwnerDiscoveryTimeoutMs});`,
      "lookup.then(owner=>{",
      "if(settled)return;",
      "settled=true;globalThis.clearTimeout(timer);",
      "resolve(owner)",
      "},error=>{",
      "if(settled)return;",
      "settled=true;globalThis.clearTimeout(timer);reject(error)",
      "})",
      "}).finally(()=>{if(requests.get(key)===request)requests.delete(key)});",
      "requests.set(key,request);",
      "return request",
      "}}",
      "})()).find(",
      `${coordinationName}.clientCoordination,${hostIdName},${conversationIdName})`,
    ].join("");
  const patchNewThreadDraftDefaults = (source) => {
    const name = "new thread draft defaults";
    if (source.includes("/*codey-new-thread-draft-defaults*/")
      || activeRendererPatchFailures?.has(name)) return source;
    // 三处必须同时识别：草稿速度初值、模型目录校验、手动草稿选择。
    // 缓存中的默认模型继续有效；旧推理强度不再成为下一个草稿的初值。
    const patterns = {
      factory: /\b[$A-Z_a-z][$\w]*=[$A-Z_a-z][$\w]*\([$A-Z_a-z][$\w]*,[$A-Z_a-z][$\w]*=>\(\{isManuallyChanged:!1,modelSettings:null,serviceTier:null\}\)\)/g,
      resolver: /function ([$A-Z_a-z][$\w]*)\(\{userSavedModelString:[$A-Z_a-z][$\w]*,userSavedReasoningEffort:[$A-Z_a-z][$\w]*,listModelsData:[$A-Z_a-z][$\w]*\}\)/g,
      catalog: /managedNewThreadModelSettings:[$A-Z_a-z][$\w]*,waitForModelList:[\s\S]{0,4000}?let\{data:([$A-Z_a-z][$\w]*),isLoading:[$A-Z_a-z][$\w]*\}=([$A-Z_a-z][$\w]*)\([$A-Z_a-z][$\w]*\),[$A-Z_a-z][$\w]*=[$A-Z_a-z][$\w]*\([$A-Z_a-z][$\w]*\?\.settings,\1\),[\s\S]{0,1500}?userSavedModelString:/g,
      model: /function [$A-Z_a-z][$\w]*\([$A-Z_a-z][$\w]*,[$A-Z_a-z][$\w]*\)\{let ([$A-Z_a-z][$\w]*)=\(0,([$A-Z_a-z][$\w]*)\.c\)\((\d+)\)[\s\S]{0,700}?let\{draftSettings:([$A-Z_a-z][$\w]*),isNewThreadDraft:[$A-Z_a-z][$\w]*,updateDraftSettings:([$A-Z_a-z][$\w]*)\}=[$A-Z_a-z][$\w]*\([$A-Z_a-z][$\w]*\),([$A-Z_a-z][$\w]*)=[$A-Z_a-z][$\w]*\([^)]*\),\{isLunaReserveActive:[$A-Z_a-z][$\w]*\}=\6,([$A-Z_a-z][$\w]*)=[$A-Z_a-z][$\w]*==null&&[$A-Z_a-z][$\w]*,[^;]{0,300}?,([$A-Z_a-z][$\w]*)=\7&&\4\.modelSettings!=null,([$A-Z_a-z][$\w]*)=\6\.modelSettings;(?=if\(\8\))/g,
    };
    const matches = {};
    for (const [key, pattern] of Object.entries(patterns)) {
      const found = [...source.matchAll(pattern)];
      if (found.length !== 1) return recordIncompatibleRendererGate(source, name, found.length);
      matches[key] = found[0];
    }
    const [, cache, , cacheSize, , update, settings, isDraft, hasOverride, model] = matches.model;
    const escapedUpdate = update.replace(/\$/g, "\\$");
    const manualPattern = new RegExp(
      `([$_A-Za-z][$\\w]*\\|\\|[$_A-Za-z][$\\w]*\\|\\|[$_A-Za-z][$\\w]*&&[$_A-Za-z][$\\w]*===\x60ultra\x60\\?)${escapedUpdate}\\([$A-Z_a-z][$\\w]*=>\\(\\{\\.\\.\\.[$A-Z_a-z][$\\w]*,isManuallyChanged:!0,modelSettings:`,
      "g",
    );
    // 只在已识别的草稿 hook 内查找回调，不能误改另一个模型选择器。
    const modelEnd = source.indexOf("}function ", matches.model.index + matches.model[0].length);
    if (modelEnd < 0) return recordIncompatibleRendererGate(source, name, 0);
    const modelScope = source.slice(matches.model.index, modelEnd + 1);
    const manualMatches = [...modelScope.matchAll(manualPattern)];
    if (manualMatches.length !== 1) return recordIncompatibleRendererGate(source, name, manualMatches.length);
    const slots = Number(cacheSize);
    const catalogHook = matches.catalog[2];
    const resolver = matches.resolver[1];
    // 新增目录 hook 始终调用，保留 React hook 顺序。未加载或模型不在目录中
    // 时保留现有设置；目录就绪后由原生解析器选择 medium 或模型支持的回退。
    const defaults = `/*codey-new-thread-draft-defaults*/let __codeyDraftCatalog=${catalogHook}({hostId:${settings}.hostId,enabled:${isDraft}}).data;if(${isDraft}&&!${hasOverride}&&__codeyDraftCatalog?.models?.some(e=>e.model===${model}.model)){let __codeyDraftEffort=${resolver}({userSavedModelString:${model}.model,userSavedReasoningEffort:\`medium\`,listModelsData:__codeyDraftCatalog}).reasoningEffort;if(${model}.reasoningEffort!==__codeyDraftEffort){let __codeyDraftSettings;${cache}[${slots}]!==${model}||${cache}[${slots + 1}]!==__codeyDraftEffort?(__codeyDraftSettings={...${model},reasoningEffort:__codeyDraftEffort},${cache}[${slots}]=${model},${cache}[${slots + 1}]=__codeyDraftEffort,${cache}[${slots + 2}]=__codeyDraftSettings):__codeyDraftSettings=${cache}[${slots + 2}],${model}=__codeyDraftSettings}}`;
    let patchedScope = modelScope.replace(matches.model[0],
      matches.model[0].replace(`.c)(${cacheSize})`, `.c)(${slots + 3})`) + defaults);
    patchedScope = patchedScope.replace(manualPattern, (match) => `${isDraft}||${match}`);
    return (source.slice(0, matches.model.index) + patchedScope + source.slice(modelEnd + 1))
      .replace(matches.factory[0], matches.factory[0].replace("serviceTier:null", "serviceTier:{value:`default`}"));
  };
  const patchCodexRendererAsset = (source) => {
    let patched = source;
    let nativeCustomProviderModelAccess = false;
    if (source.includes("isNewThreadDraft:")
      && source.includes("modelSettingsPersistAsDefault:")
      && source.includes("userSavedReasoningEffort:")) {
      patched = patchNewThreadDraftDefaults(patched);
    }
    if (!source.includes("/*codey-app-server-manager-registry*/")
      && source.includes("inactiveThreadUnsubscriber.clearConversationStreamOwnership")
      && source.includes("getStreamRole(")
      && source.includes("updateConversationState(")) {
      // 在原生管理器被读取时登记真实实例，避免依赖模块导出名和 React 结构。
      // 登记失败不得影响原生读取，也不允许已销毁实例覆盖新管理器。
      patched = replaceUniqueRendererGate(
        patched,
        /getConversation\(([$A-Z_a-z][$\w]*)\)\{return this\.threadStore\.getConversation\(\1\)\}/g,
        (_match, id) => `getConversation(${id}){/*codey-app-server-manager-registry*/try{if(this.disposed!==true&&this.getHostId()===\`local\`){let __codeyManagers=globalThis.__codeyAppServerManagers;if(__codeyManagers==null)globalThis.__codeyAppServerManagers=__codeyManagers=new Map;if(__codeyManagers instanceof Map)__codeyManagers.set(\`local\`,this)}}catch{}return this.threadStore.getConversation(${id})}`,
        "app server manager registry",
      );
    }
    if (source.includes("codexComposerAdapter.CodexComposerAdapter.interruptThreadError")
      && !source.includes("/*codey-local-quota-gate*/")) {
      // 只改变 Codex 本机发送按钮的额度条件。保留共享额度状态、官方警告
      // 和其他禁用条件；每次渲染读取当前模型，避免模型切换后复用旧判断。
      patched = replaceUniqueRendererGate(
        patched,
        /let\{agentMode:[^;]{0,1024}?(?<mode>[$A-Z_a-z][$\w]*)\.settings\.model===`gpt-reserve`[^;]{0,1024}?,(?<quota>[$A-Z_a-z][$\w]*)=[$A-Z_a-z][$\w]*\((?<host>[$A-Z_a-z][$\w]*)===`local`&&[$A-Z_a-z][$\w]*\)(?=,[\s\S]{0,10000}?activeCollaborationMode:\k<mode>,[\s\S]{0,2000}?resolvedHostId:\k<host>,[\s\S]{0,10000}?let (?<disabled>[$A-Z_a-z][$\w]*)=[^;]{0,512}?\|\|\k<quota>,[\s\S]{0,6000}?submitDisabled:\k<disabled>,)/g,
        (...args) => {
          const { host, mode } = args.at(-1);
          return `${args[0]}&&/*codey-local-quota-gate*/(()=>{try{return globalThis.__codeyQuotaUnlock?.allowsLocalSend?.({hostId:${host},model:${mode}.settings.model})!==true}catch{return true}})()`;
        },
        "local third-party composer quota",
      );
    }
    if (
      source.includes("codex-message-from-view")
      && source.includes("sendMessageFromView")
      && source.includes("Failed to send message from view")
    ) {
      // The native renderer forwards the request through Electron before it
      // emits codex-message-from-view. Event-only injections therefore see an
      // already-sent payload. Guard at the bridge boundary and rewrite only
      // after preparation. The guard suppresses early observational tracking
      // while pending; transport failures retain the renderer's original catch.
      patched = replaceUniqueRendererGate(
        patched,
        /if\(([$A-Z_a-z][$\w]*)\?\.sendMessageFromView\)\{let ([$A-Z_a-z][$\w]*)=([$A-Z_a-z][$\w]*);\1\.sendMessageFromView\(\2\)\.catch\(([$A-Z_a-z][$\w]*)=>\{/g,
        (_match, bridgeName, messageName, sourceName, errorName) =>
          `if(${bridgeName}?.sendMessageFromView){let ${messageName}=${sourceName};let __codeySend=()=>{${messageName}=globalThis.__codeyModelWhitelistPatch?.rewriteOutgoingMessage?.(${sourceName})??${sourceName};if(globalThis.__codeyModelWhitelistPatch?.isBlockedOutgoingMessage?.(${messageName})){globalThis.__codeyModelWhitelistPatch?.notifyBlockedOutgoingMessage?.(${messageName});return Promise.resolve()}return ${bridgeName}.sendMessageFromView(${messageName})};let __codeyGuard=globalThis.__codeyModelWhitelistPatch?.guardDirectBridgeMessage?.(${sourceName},__codeySend);(__codeyGuard==null||__codeyGuard===true?__codeySend():__codeyGuard).catch(${errorName}=>{`,
        "model route bridge preflight",
      );
    }
    if (
      source.includes("AppServerRequestClient is missing a message dispatcher")
      && source.includes("mcp_request_enqueued")
      && source.includes("this.dispatchMessage?.(`mcp-request`")
    ) {
      // Current Codex can create threads through AppServerRequestClient without
      // touching the renderer bridge helper above. Rewrite at enqueue time so
      // thread/start and prewarm requests bind the selected Codey route before
      // they reach the app server. Keep the live client so MCP reload can send
      // config/mcpServer/reload without walking the React tree.
      patched = replaceUniqueRendererGate(
        patched,
        /(enqueueRequest\(([$A-Z_a-z][$\w]*),([$A-Z_a-z][$\w]*),([$A-Z_a-z][$\w]*),([$A-Z_a-z][$\w]*)=[$A-Z_a-z][$\w]*=>\{this\.dispatchMessage\?\.\(`mcp-request`,\{request:[$A-Z_a-z][$\w]*,hostId:this\.hostId,[\s\S]{0,700}?widget:\4\?\.widget\}\)\},[$A-Z_a-z][$\w]*=null\)\{)let /g,
        (_match, prefix, methodName, paramsName) =>
          `${prefix}let __codeyGuard=globalThis.__codeyModelWhitelistPatch?.guardOutgoingRequest?.(this,${methodName},${paramsName},()=>this.enqueueRequest(...arguments));if(__codeyGuard)return __codeyGuard;(globalThis.__codeyAppServerRequestClients??(globalThis.__codeyAppServerRequestClients=new Map)).set(this.hostId,this);let __codeyRoute=globalThis.__codeyModelWhitelistPatch?.rewriteOutgoingMessage?.({type:\`mcp-request\`,hostId:this.hostId,request:{method:${methodName},params:${paramsName}}});if(__codeyRoute?.request){if(globalThis.__codeyModelWhitelistPatch?.isBlockedOutgoingMessage?.(__codeyRoute)){globalThis.__codeyModelWhitelistPatch?.notifyBlockedOutgoingMessage?.(__codeyRoute);return Promise.reject(Error(\`Codey blocked cross-provider model request\`))}${methodName}=__codeyRoute.request.method??${methodName},${paramsName}=__codeyRoute.request.params??${paramsName}}let `,
        "app server request route preflight",
      );
      // AppServerRequestClient runs the preflight before createRequest assigns
      // an id. Register the concrete request afterwards so a successful legacy
      // OpenAI resume is remembered as a codey_router migration when its reply
      // still exposes the rollout's persisted `openai` provider.
      patched = replaceUniqueRendererGate(
        patched,
        /(let\{request:([$A-Z_a-z][$\w]*),promise:[$A-Z_a-z][$\w]*\}=this\.createRequest\([^;]{1,256}\);)/g,
        (_match, createRequest, requestName) =>
          `${createRequest}globalThis.__codeyModelWhitelistPatch?.trackOutgoingMessage?.({type:\`mcp-request\`,hostId:this.hostId,request:${requestName}});`,
        "app server request identity tracking",
      );
      // Promise consumers update React Query before the diagnostic response
      // event runs. Rewrite model/list at the resolver boundary so the native
      // result can never replace Codey's current route catalog.
      patched = replaceUniqueRendererGate(
        patched,
        /(([$A-Z_a-z][$\w]*)\.resolve\()([$A-Z_a-z][$\w]*)(\),this\.emitRequestLifecycleEvent\(\{type:`completed`,hostId:this\.hostId,method:\2\.method)/g,
        (_match, prefix, requestName, resultName, suffix) =>
          `${prefix}${resultName}=globalThis.__codeyModelWhitelistPatch?.rewriteIncomingResult?.(${requestName}.method,${resultName},this.hostId)??${resultName}${suffix}`,
        "app server model result rewrite",
      );
    }
    if (
      disablePet
      && /settings\.(?:(?:appearance|personalization)\.)?(?:pets|mini)(?:[."`]|$)/.test(source)
      && petRendererResourceImport.test(source)
    ) {
      // Codex keeps the pets/Mini settings preview in a settings route chunk
      // and statically imports the avatar renderer, mascot button, pet asset
      // table, and avatar option hook from it. Hiding the controls after React
      // mounts is too late: those imports have already pulled every bundled
      // spritesheet into the main window. Replace each settings-side dependency
      // with inert callable/iterable bindings. The shared avatar overlay host
      // stays intact because current Codex builds also use it for voice
      // controls.
      patched = replacePetRendererResourceImports(
        patched,
        "pet settings avatar resources",
      );
    }
    if (
      source.includes("maybe_resume_owner_discovery_failed")
      && source.includes("followExistingOwner")
      && source.includes(".clientCoordination.findThreadOwner")
    ) {
      // Owner discovery is an optimization for reusing a stream already owned
      // by another window. Merge only duplicate in-flight lookups: a settled
      // positive answer can become stale as soon as its owner disconnects, and
      // reusing it would mark this renderer as a follower without receiving a
      // snapshot. Every later hydration attempt revalidates the live owner.
      // Lookups retain a short safety window before local hydration.
      patched = replaceUniqueRendererGate(
        patched,
        /await\s+([$A-Z_a-z][$\w]*)\.clientCoordination\.findThreadOwner\(\{\s*hostId\s*:\s*([$A-Z_a-z][$\w]*)\s*,\s*conversationId\s*:\s*([$A-Z_a-z][$\w]*)\s*\}\)/g,
        (_match, coordinationName, hostIdName, conversationIdName) =>
          threadOwnerDiscoveryExpression(
            coordinationName,
            hostIdName,
            conversationIdName,
          ),
        "thread owner discovery coalescing",
      );
    }
    if (
      !source.includes("codeyReconcileCompletedConversation")
      && source.includes("isLocalConversationInProgress")
      && source.includes("inactiveThreadUnsubscriber.clearConversationStreamOwnership")
      && source.includes("getConversationStreamRevision")
      && source.includes("async resumeConversation(")
    ) {
      // A renderer can miss the terminal notification while retaining its old
      // in-progress turn and stream role. Confirm the native app-server state
      // twice, reject a concurrent stream revision or follower window, then use
      // the same needs_resume + maybeResumeConversation path as reconnect. This
      // hydrates paginated history without discarding, interrupting, or starting
      // another turn.
      patched = replaceUniqueRendererGate(
        patched,
        /async resumeConversation\(([$A-Z_a-z][$\w]*)\)\{await this\.maybeResumeConversation\(\1\);/g,
        (_match, paramsName) =>
          `async codeyReconcileCompletedConversation(${paramsName}){let __codeyConversationId=${paramsName}?.conversationId,__codeyConversation=__codeyConversationId==null?null:this.getConversation(__codeyConversationId);if(__codeyConversationId==null||__codeyConversation==null||!this.productPolicy.runtimePolicy.isLocalConversationInProgress(__codeyConversation)||this.getStreamRole(__codeyConversationId)?.role===\`follower\`)return!1;let __codeyRevision=this.getConversationStreamRevision(__codeyConversationId),__codeyReadStatus=async()=>{let __codeyResponse=await this.sendRequest(\`thread/read\`,{threadId:__codeyConversationId,includeTurns:!1}),__codeyStatus=__codeyResponse?.thread?.status;return typeof __codeyStatus===\`string\`?__codeyStatus:__codeyStatus?.type},__codeyStatus=await __codeyReadStatus();if(__codeyStatus!==\`idle\`&&__codeyStatus!==\`error\`)return!1;await new Promise(__codeyResolve=>setTimeout(__codeyResolve,250));if(await __codeyReadStatus()!==__codeyStatus||this.getConversationStreamRevision(__codeyConversationId)!==__codeyRevision)return!1;__codeyConversation=this.getConversation(__codeyConversationId);if(__codeyConversation==null||!this.productPolicy.runtimePolicy.isLocalConversationInProgress(__codeyConversation)||this.getStreamRole(__codeyConversationId)?.role===\`follower\`)return!1;this.inactiveThreadUnsubscriber.clearConversationStreamOwnership(__codeyConversationId);this.updateConversationState(__codeyConversationId,__codeyState=>{__codeyState.resumeState=\`needs_resume\`},!1);await this.maybeResumeConversation(${paramsName});__codeyConversation=this.getConversation(__codeyConversationId);return __codeyConversation!=null&&!this.productPolicy.runtimePolicy.isLocalConversationInProgress(__codeyConversation)}async resumeConversation(${paramsName}){await this.maybeResumeConversation(${paramsName});`,
        "completed thread reconciliation",
      );
    }
    if (
      source.includes("useHiddenModels:") &&
      source.includes("availableModels:") &&
      source.includes("includeUltraReasoningEffort") &&
      source.includes("amazonBedrock")
    ) {
      // Newer Codex builds already bypass the native allowlist for custom
      // providers and fall back to the model's own visibility bit. Recognize
      // that semantic shape as compatible instead of logging a false failure.
      nativeCustomProviderModelAccess =
        rendererHasNativeCustomProviderModelAccess(source);
      if (!nativeCustomProviderModelAccess) {
        patched = replaceUniqueRendererGate(
          patched,
          /if\s*\(\s*\(*\s*(?:[$A-Z_a-z][$\w]*\s*(?:\?\.|\.)\s*has\(\s*[$A-Z_a-z][$\w]*\.model\s*\)\s*(?:===\s*!0)?\s*\|\|\s*)?\(?\s*([$A-Z_a-z][$\w]*)\s*\?\s*([$A-Z_a-z][$\w]*)\.has\(\s*([$A-Z_a-z][$\w]*)\.model\s*\)\s*:\s*(?:!\s*\3\.hidden|\3\.hidden\s*!==\s*!0|\3\.hidden\s*===\s*!1)\s*\)?\s*\)*\s*\)/g,
          (_match, useAllowlistName, allowlistName, modelName) =>
            `if(${useAllowlistName}?(${allowlistName}.has(${modelName}.model)||!${modelName}.hidden):!${modelName}.hidden)`,
          "model allowlist",
        );
      }
    }
    if (
      source.includes("useHiddenModels:") &&
      source.includes("includeUltraReasoningEffort") &&
      source.includes("amazonBedrock") &&
      !nativeCustomProviderModelAccess
    ) {
      patched = replaceUniqueRendererGate(
        patched,
        /(\b[$A-Z_a-z][$\w]*\s*=\s*\(?\s*[$A-Z_a-z][$\w]*(?:\s*(?:\?\.|\.)\s*[$A-Z_a-z][$\w]*)?\s*\)?\s*&&\s*)\(?\s*([$A-Z_a-z][$\w]*(?:\s*(?:\?\.|\.)\s*[$A-Z_a-z][$\w]*)?)\s*(?:!==|!=)\s*(["'`])amazonBedrock\3\s*\)?/g,
        (_match, visibilityPrefix, authMethodExpression) =>
          `${visibilityPrefix}${authMethodExpression}=== \`chatgpt\``,
        "model visibility",
      );
    }
    if (
      source.includes("includeUltraReasoningEffort:") &&
      source.includes("isCustomModelProvider:") &&
      source.includes("1186680773")
    ) {
      // 第三方线路按模型目录显示 Ultra，保留调用方和用户的推理等级设置。
      patched = replaceUniqueRendererGate(
        patched,
        /(\(\{[^{}]*\bincludeUltraReasoningEffort\s*:\s*([$A-Z_a-z][$\w]*)[^{}]*\bisCustomModelProvider\s*:\s*([$A-Z_a-z][$\w]*)[^{}]*\}\s*,\s*\{[^{}]*\bget\s*:\s*([$A-Z_a-z][$\w]*)[^{}]*\}\)\s*=>\s*\{[^{}]*\b[$A-Z_a-z][$\w]*\s*=\s*\2\s*&&\s*)(\4\(\s*[$A-Z_a-z][$\w]*\s*,\s*(["'`])1186680773\6\s*\))/g,
        (_match, prefix, _includeUltra, customProvider, _get, gate) =>
          `${prefix}(${customProvider}||${gate})`,
        "third-party Ultra reasoning",
      );
    }
    if (
      source.includes("isServiceTierAllowed") &&
      source.includes("featureRequirements?.fast_mode") &&
      source.includes("authMethod:")
    ) {
      // Model serviceTiers are the authority for whether the control exists.
      // Account requirements and their loading state must never hide it.
      patched = replaceUniqueRendererGate(
        patched,
        /(\b([$A-Z_a-z][$\w]*)\s*=\s*)([$A-Z_a-z][$\w]*)\s*&&\s*!([$A-Z_a-z][$\w]*)\s*&&\s*([$A-Z_a-z][$\w]*)\s*!=\s*null\s*&&\s*\5\?\.requirements\?\.featureRequirements\?\.fast_mode\s*!==\s*!1/g,
        (_match, assignment) => `${assignment}!0`,
        "service tier UI",
      );
    }
    if (
      source.includes("supportsIndependentSpeedModes") &&
      source.includes("ultrafast_mode")
    ) {
      // Ultrafast 与 Fast 是两条独立的授权字段。上一条只放开了 fast_mode，
      // 这里把 ultrafast_mode 的判定一并收敛，避免上游把它默认置否后
      // 速度档控件静默消失。
      patched = replaceUniqueRendererGate(
        patched,
        /(\bultrafast\s*:\s*)\([^;{}\n]{0,400}?\)\s*&&\s*[$A-Z_a-z][$\w]*\s*\?\.\s*ultrafast_mode\s*!==\s*!1/g,
        (_match, prefix) => `${prefix}!0`,
        "ultrafast tier entitlement",
      );
    }
    if (
      source.includes("isServiceTierAllowed") &&
      source.includes("serviceTierForRequest:") &&
      source.includes("availableOptions:")
    ) {
      // Preserve the model-aware resolver but remove its entitlement argument.
      // This also covers builds where the permission provider above reshaped.
      patched = replaceUniqueRendererGate(
        patched,
        /(\?\s*)([$A-Z_a-z][$\w]*)\s*\?\s*([$A-Z_a-z][$\w]*)\s*:\s*null\s*:\s*([$A-Z_a-z][$\w]*)\(\s*([$A-Z_a-z][$\w]*)\s*,\s*\3\s*,\s*\2\s*\)/g,
        (_match, _questionMark, _isAllowedName, tierName, resolverName, modelName) =>
          `?${tierName}:${resolverName}(${modelName},${tierName})`,
        "service tier selection permission",
      );
      // Reuse Codex's normalized selected tier for the request too. A Fast tier
      // left over from another model must become null after switching to a
      // model whose serviceTiers do not contain it.
      patched = replaceUniqueRendererGate(
        patched,
        /(\b([$A-Z_a-z][$\w]*)\s*=\s*([$A-Z_a-z][$\w]*)\s*==\s*null\s*\?\s*null\s*:\s*([$A-Z_a-z][$\w]*)\(\s*([$A-Z_a-z][$\w]*)\s*,\s*\3\s*\))(?=\s*;\s*let\s+[$A-Z_a-z][$\w]*\s*=\s*[$A-Z_a-z][$\w]*\(\s*\3\s*\?\?\s*null\s*\))/g,
        (_match, selectedExpression, selectedName, requestTierName) =>
          `${selectedExpression},${requestTierName}=${selectedName}`,
        "service tier model validation",
      );
      // Requirements can remain pending independently of the model catalog.
      // Do not report that entitlement fetch as service-tier option loading.
      patched = replaceUniqueRendererGate(
        patched,
        /(\b([$A-Z_a-z][$\w]*)\s*=\s*([$A-Z_a-z][$\w]*)\.isLoading\s*\|\|\s*([$A-Z_a-z][$\w]*)\s*\|\|\s*([$A-Z_a-z][$\w]*)\.isLoading)\s*\|\|\s*[$A-Z_a-z][$\w]*\s*==\s*null\s*&&\s*[$A-Z_a-z][$\w]*(?=\s*,)/g,
        (_match, modelLoadingExpression) => modelLoadingExpression,
        "service tier entitlement loading",
      );
    }
    if (
      source.includes("composer.toggleFastMode") &&
      source.includes("isServiceTierAllowed") &&
      source.includes("availableOptions.length")
    ) {
      // 速度档入口不再跟随当前模型声明的档位数量，始终保留在输入区。
      patched = replaceNearestRendererGateBeforeAnchor(
        patched,
        [
          {
            pattern: /(\b([$A-Z_a-z][$\w]*)\s*=\s*)\(?\s*([$A-Z_a-z][$\w]*)\.availableOptions\.length\s*>\s*1\s*\)?\s*&&\s*!\s*([$A-Z_a-z][$\w]*)\s*&&\s*[$A-Z_a-z][$\w]*(?=\s*[,;][\s\S]{0,8192}?`composer\.toggleFastMode`)/g,
            replacement: (_match, assignment, _resultName, _settingsName, draftName) =>
              `${assignment}!${draftName}`,
          },
          {
            pattern: /(\b([$A-Z_a-z][$\w]*)\s*=\s*)\(?\s*([$A-Z_a-z][$\w]*)\.availableOptions\.length\s*>\s*1\s*\)?\s*&&\s*[$A-Z_a-z][$\w]*\s*&&\s*!\s*([$A-Z_a-z][$\w]*)(?=\s*[,;][\s\S]{0,8192}?`composer\.toggleFastMode`)/g,
            replacement: (_match, assignment, _resultName, _settingsName, draftName) =>
              `${assignment}!${draftName}`,
          },
          {
            pattern: /(\b([$A-Z_a-z][$\w]*)\s*=\s*)\(?\s*([$A-Z_a-z][$\w]*)\.availableOptions\.length\s*>\s*1\s*\)?\s*&&\s*[$A-Z_a-z][$\w]*(?!\s*&&\s*!)(?=\s*[,;][\s\S]{0,8192}?`composer\.toggleFastMode`)/g,
            replacement: (_match, assignment) => `${assignment}!0`,
          },
          {
            pattern: /(\b([$A-Z_a-z][$\w]*)\s*=\s*!\s*([$A-Z_a-z][$\w]*)\s*&&\s*)\(?\s*([$A-Z_a-z][$\w]*)\.availableOptions\.length\s*>\s*1\s*\)?\s*&&\s*[$A-Z_a-z][$\w]*(?=\s*[,;][\s\S]{0,8192}?`composer\.toggleFastMode`)/g,
            replacement: (_match, preservedPrefix) => `${preservedPrefix}!0`,
          },
          {
            pattern: /(\b([$A-Z_a-z][$\w]*)\s*=\s*)[$A-Z_a-z][$\w]*\s*&&\s*!\s*([$A-Z_a-z][$\w]*)\s*&&\s*\(?\s*([$A-Z_a-z][$\w]*)\.availableOptions\.length\s*>\s*1\s*\)?(?=\s*[,;][\s\S]{0,8192}?`composer\.toggleFastMode`)/g,
            replacement: (
              _match,
              assignment,
              _resultName,
              draftName,
              _settingsName,
            ) => `${assignment}!${draftName}`,
          },
          {
            pattern: /(\b([$A-Z_a-z][$\w]*)\s*=\s*)[$A-Z_a-z][$\w]*\s*&&\s*\(?\s*([$A-Z_a-z][$\w]*)\.availableOptions\.length\s*>\s*1\s*\)?\s*&&\s*!\s*([$A-Z_a-z][$\w]*)(?=\s*[,;][\s\S]{0,8192}?`composer\.toggleFastMode`)/g,
            replacement: (
              _match,
              assignment,
              _resultName,
              _settingsName,
              draftName,
            ) => `${assignment}!${draftName}`,
          },
          {
            pattern: /(\b([$A-Z_a-z][$\w]*)\s*=\s*)[$A-Z_a-z][$\w]*\s*&&\s*([$A-Z_a-z][$\w]*)\.availableOptions\.length\s*>\s*1(?=\s*[,;][\s\S]{0,8192}?`composer\.toggleFastMode`)/g,
            replacement: (_match, assignment) => `${assignment}!0`,
          },
          {
            pattern: /(\b([$A-Z_a-z][$\w]*)\s*=\s*!\s*([$A-Z_a-z][$\w]*)\s*&&\s*)[$A-Z_a-z][$\w]*\s*&&\s*([$A-Z_a-z][$\w]*)\.availableOptions\.length\s*>\s*1(?=\s*[,;][\s\S]{0,8192}?`composer\.toggleFastMode`)/g,
            replacement: (_match, preservedPrefix) => `${preservedPrefix}!0`,
          },
        ],
        undefined,
        "model-aware service tier control",
        "`composer.toggleFastMode`",
        8192,
      );
      if (source.includes("!=null")) {
        patched = replaceUniqueRendererGate(
          patched,
          [
            {
              pattern: /(`composer\.toggleFastMode`[\s\S]{0,4096}?\{\s*enabled\s*:\s*)[$A-Z_a-z][$\w]*\s*&&\s*!\s*([$A-Z_a-z][$\w]*)\s*&&\s*([$A-Z_a-z][$\w]*)\s*!=\s*null/g,
              replacement: (_match, prefix, loadingName, fastOptionName) =>
                `${prefix}!${loadingName}&&${fastOptionName}!=null`,
            },
            {
              pattern: /(\b([$A-Z_a-z][$\w]*)\s*=\s*!\s*([$A-Z_a-z][$\w]*)\s*&&\s*)[$A-Z_a-z][$\w]*\s*&&\s*!\s*([$A-Z_a-z][$\w]*)\s*&&\s*([$A-Z_a-z][$\w]*)\s*!=\s*null(?=\s*[,;][\s\S]{0,4096}?\{\s*enabled\s*:\s*\2\s*\}[\s\S]{0,4096}?`composer\.toggleFastMode`)/g,
              replacement: (
                _match,
                preservedPrefix,
                _resultName,
                _draftName,
                loadingName,
                fastOptionName,
              ) => `${preservedPrefix}!${loadingName}&&${fastOptionName}!=null`,
            },
          ],
          undefined,
          "model-aware Fast toggle",
        );
      }
    }
    if (
      source.includes("composer.speedSlashCommand.disableDescription") &&
      source.includes("isServiceTierAllowed") &&
      source.includes("availableOptions.map")
    ) {
      // These commands are created only for service tiers exposed by the model.
      patched = replaceUniqueRendererGate(
        patched,
        /(enabled\s*:\s*)[$A-Z_a-z][$\w]*\s*&&\s*!\s*([$A-Z_a-z][$\w]*)\.isLoading(?=\s*,\s*isSelected\s*:)/g,
        (_match, assignment, settingsName) =>
          `${assignment}!${settingsName}.isLoading`,
        "model-aware service tier commands",
      );
    }
    if (
      source.includes("isServiceTierAllowed") &&
      /availableOptions\.length\s*<=\s*1/.test(source) &&
      source.includes("selectedServiceTier")
    ) {
      patched = replaceUniqueRendererGate(
        patched,
        /if\s*\(\s*!\s*([$A-Z_a-z][$\w]*)\s*\|\|\s*([$A-Z_a-z][$\w]*)\.availableOptions\.length\s*<=\s*1\s*\)\s*return\s+null/g,
        (_match, _isAllowedName, settingsName) =>
          `if(${settingsName}.availableOptions.length<=1)return null`,
        "service tier settings UI",
      );
    }
    if (
      source.includes("Failed to load config requirements for service tier") &&
      source.includes("featureRequirements?.fast_mode")
    ) {
      // A tier selected from the current model must not be stripped from thread
      // requests by an account entitlement lookup.
      patched = replaceUniqueRendererGate(
        patched,
        /if\s*\(\s*\(\s*await\s+([$A-Z_a-z][$\w]*)\(\s*\)\s*\)\.requirements\?\.featureRequirements\?\.fast_mode\s*===\s*!1\s*\)\s*return\s+null/g,
        "",
        "service tier request sanitizer",
      );
    }
    if (
      source.includes("Failed to read service tier for request") &&
      source.includes("featureRequirements?.fast_mode")
    ) {
      patched = replaceUniqueRendererGate(
        patched,
        /async\s+function\s+([$A-Z_a-z][$\w]*)\(\s*([$A-Z_a-z][$\w]*)\s*,\s*([$A-Z_a-z][$\w]*)\s*\)\s*\{\s*let\s+([$A-Z_a-z][$\w]*)\s*=\s*await\s+[$A-Z_a-z][$\w]*\(\s*\2\s*,\s*\3\s*\)\s*;\s*if\s*\(\s*\4\s*!==\s*`chatgpt`\s*\)\s*return\s*!1\s*;[\s\S]{0,768}?\.requirements\?\.featureRequirements\?\.fast_mode\s*!==\s*!1\s*\}/g,
        (_match, functionName, firstArgumentName, secondArgumentName) =>
          `async function ${functionName}(${firstArgumentName},${secondArgumentName}){return!0}`,
        "service tier request entitlement",
      );
    }
    if (
      source.includes("composer.intelligenceDropdown.model.title") &&
      source.includes("modelPickerTriggerConfig:") &&
      source.includes("selectedServiceTierIconKind:")
    ) {
      // Use the modern trigger for every route and model. Newer builds removed
      // rowLabel/showFastServiceTierIndicator and memoize the trigger config.
      patched = replaceUniqueRendererGate(
        patched,
        [
          {
            pattern: /(\b([$A-Z_a-z][$\w]*)\s*=\s*)[$A-Z_a-z][$\w]*\s*&&\s*!\s*([$A-Z_a-z][$\w]*)(?=\s*,[\s\S]{0,8192}?modelPickerTriggerConfig\s*:\s*\2\s*\?)/g,
            replacement: (
              _match,
              assignment,
              _triggerConfigName,
              hideLabelName,
            ) => `${assignment}!${hideLabelName}`,
          },
          {
            pattern: /(\b([$A-Z_a-z][$\w]*)\s*=\s*)[$A-Z_a-z][$\w]*\s*&&\s*!\s*([$A-Z_a-z][$\w]*)(?=\s*,[\s\S]{0,12288}?\b([$A-Z_a-z][$\w]*)\s*=\s*\2\s*\?\s*\{[\s\S]{0,2048}?selectedServiceTierIconKind\s*:[\s\S]{0,8192}?modelPickerTriggerConfig\s*:\s*\4\b)/g,
            replacement: (
              _match,
              assignment,
              _triggerConfigName,
              hideLabelName,
            ) => `${assignment}!${hideLabelName}`,
          },
        ],
        undefined,
        "fast model trigger availability",
      );
      // Preserve Codex's native Fast indicators. Its own model/tier support
      // checks already prevent them from appearing on unsupported models.
      patched = replaceUniqueRendererGate(
        patched,
        /(modelPickerTriggerConfig\s*:\s*([$A-Z_a-z][$\w]*)\s*[,}][\s\S]{0,2048}?selectedServiceTierIconKind\s*:[\s\S]{0,12288}?)if\s*\(\s*[$A-Z_a-z][$\w]*\s*&&\s*\2\s*!=\s*null\s*\)|if\s*\(\s*[$A-Z_a-z][$\w]*\s*&&\s*modelPickerTriggerConfig\s*!=\s*null\s*\)/g,
        (_match, aliasedPrefix, triggerConfigName) =>
          aliasedPrefix == null
            ? "if(modelPickerTriggerConfig!=null)"
            : `${aliasedPrefix}if(${triggerConfigName}!=null)`,
        "fast model trigger fallback",
      );
    }
    if (
      source.includes("activeInteractions=new Map") &&
      source.includes("beginCpuSampling") &&
      source.includes(
        "ensureHeartbeat(){this.heartbeatTimer??=setInterval",
      ) &&
      source.includes("rendererProcessCpuPercentAvg")
    ) {
      // Codey launches app-server with analytics.enabled=false, so renderer
      // interaction telemetry is discarded after paying for main/renderer CPU
      // snapshots and a 1 Hz heartbeat. Preserve span lifecycle semantics while
      // removing only those two recurring/IPC costs.
      patched = replaceUniqueRendererGate(
        patched,
        /cpuSampling:([$A-Z_a-z][$\w]*)===`dropped`\|\|([$A-Z_a-z][$\w]*)\.backfilled===!0\?null:this\.beginCpuSampling\(\)/g,
        "cpuSampling:null",
        "interaction CPU sampling",
      );
      patched = replaceUniqueRendererGate(
        patched,
        /ensureHeartbeat\(\)\{this\.heartbeatTimer\?\?=setInterval\(\(\)=>\{let ([$A-Z_a-z][$\w]*)=this\.now\(\),([$A-Z_a-z][$\w]*)=this\.wallNow\(\);for\(let ([$A-Z_a-z][$\w]*) of this\.activeInteractions\.values\(\)\)this\.recordHeartbeat\(\3,\1,\2\)\},([$A-Z_a-z][$\w]*)\)\}/g,
        "ensureHeartbeat(){}",
        "interaction heartbeat",
      );
    }
    if (patched.includes("globalThis.__codeyModelWhitelistPatch?.trackOutgoingMessage")
      && patched.includes("let __codeyRoute=globalThis.__codeyModelWhitelistPatch?.rewriteOutgoingMessage")
      && patched.includes("globalThis.__codeyModelWhitelistPatch?.rewriteIncomingResult")) {
      patched += "\n;globalThis.__codeyModelRequestSourceGateInstalled=true;\n";
    }
    return patched;
  };
  const discoveredCodexRendererAssets = new Set();
  const maximumDiscoveredCodexRendererAssets = 128;
  const rememberCodexRendererAsset = (baseUrl, specifier) => {
    try {
      const url = new URL(specifier, baseUrl);
      if (
        url.protocol !== "app:" ||
        !url.pathname.includes("/assets/") ||
        !/\.(?:c|m)?js$/i.test(url.pathname)
      ) return;
      discoveredCodexRendererAssets.delete(url.pathname);
      discoveredCodexRendererAssets.add(url.pathname);
      while (
        discoveredCodexRendererAssets.size >
        maximumDiscoveredCodexRendererAssets
      ) {
        const oldest = discoveredCodexRendererAssets.keys().next().value;
        if (oldest === undefined) break;
        discoveredCodexRendererAssets.delete(oldest);
      }
    } catch {}
  };
  const discoverCodexRendererAssets = (baseUrl, source) => {
    for (const match of source.matchAll(
      /\b(?:src|href)\s*=\s*(["'])([^"']+\.(?:c|m)?js(?:[?#][^"']*)?)\1/gi,
    )) rememberCodexRendererAsset(baseUrl, match[2]);
  };
  const isCodexRendererBootstrapUrl = (requestUrl) => {
    try {
      const url = new URL(requestUrl);
      return url.protocol === "app:" && /\/index\.html$/i.test(url.pathname);
    } catch {
      return false;
    }
  };
  const isCodexRendererAssetUrl = (requestUrl) => {
    try {
      const url = new URL(requestUrl);
      return (
        url.protocol === "app:" &&
        url.pathname.includes("/assets/") &&
        (
          /\/(?:(?:app-initial|app-primary|app-shared|codex-composer-adapter|general-settings|model-list-filter|windows-model-controls|use-service-tier-settings|read-service-tier-for-request|subagent-activity-chip-group)(?:[~-][^/]*)?)\.(?:c|m)?js$/i.test(
            url.pathname,
          ) ||
          (
            disablePet &&
            /\/(?:(?:appearance-settings|pet-settings|pets-settings)(?:[~-][^/]*)?)\.(?:c|m)?js$/i.test(
              url.pathname,
            )
          ) ||
          discoveredCodexRendererAssets.has(url.pathname)
        )
      );
    } catch {
      return false;
    }
  };
  // CDP Fetch 拦截入口：输入响应体文本，返回改写后的文本；null 表示放行原响应。
  // index.html 只用于发现资产清单，不写回。
  globalThis.__codeyRendererPatchEntry = (requestUrl, source) => {
    try {
      if (typeof requestUrl !== "string" || typeof source !== "string") {
        return null;
      }
      if (isCodexRendererBootstrapUrl(requestUrl)) {
        try {
          discoverCodexRendererAssets(requestUrl, source);
        } catch (error) {
          recordCodeyPatchFailure("renderer_patch:asset discovery", error, {
            requestUrl,
          });
        }
        return null;
      }
      if (!isCodexRendererAssetUrl(requestUrl)) return null;
      let patched;
      const fingerprint = rendererPatchFingerprint(source);
      if (
        fingerprint != null &&
        rendererPatchedOutputByFingerprint.has(fingerprint)
      ) {
        patched = rendererPatchedOutputByFingerprint.get(fingerprint);
        // Refresh insertion order so the bounded map behaves as an LRU.
        rememberRendererPatchedOutput(fingerprint, patched);
        return patched === source ? null : patched;
      }
      const previousRendererPatchFailures = activeRendererPatchFailures;
      activeRendererPatchFailures = rendererPatchFailuresForFingerprint(fingerprint);
      try {
        patched = patchCodexRendererAsset(source);
        rememberRendererPatchedOutput(fingerprint, patched);
      } catch (error) {
        // Codex renderer bundles are minified implementation details and their
        // shapes change between releases. These UI restorations are optional:
        // never turn a stale patch anchor into a failed app:// module request,
        // otherwise Codex remains on its static startup loader forever.
        recordCodeyPatchFailure("patch_codex_renderer_asset", error, {
          requestUrl,
        });
        try {
          console.error("Codey skipped an incompatible Codex renderer patch", error);
        } catch {}
        return null;
      } finally {
        activeRendererPatchFailures = previousRendererPatchFailures;
      }
      return patched === source ? null : patched;
    } catch (error) {
      try {
        console.error("[Codey] renderer asset patch entry failed", error);
      } catch {}
      return null;
    }
  };
  globalThis.__codeyRendererPatchInstalled = true;
})();
