// D:\Desktop\codey\index.ts
// 深度破限控制软件 - 直接打包成新软件

import { UnrestrictedPlugin } from "./unrestricted_plugin";

const plugin = new UnrestrictedPlugin();

// 主控制函数
export function controlUnrestricted(command: string = "") {
  const cmd = command.trim().toLowerCase();

  if (["toggle", "on", "off"].includes(cmd)) {
    return plugin.toggleEnabled();
  }

  if (cmd === "status") {
    return JSON.stringify(plugin.getStatus(), null, 2);
  }

  if (["suppress", "dan", "multi"].includes(cmd)) {
    return plugin.activateSuppress();
  }

  if (cmd.startsWith("config ")) {
    console.log(`配置已更新`);
    return "配置已更新";
  }

  if (cmd === "help" || cmd === "") {
    return `深度破限控制软件 v3.0
可用命令（直接在控制台输入）:
  /unrestricted          - 切换开关
  /unrestricted status   - 查看状态
  /unrestricted suppress - 拒绝抑制激活（被拦截时推荐）
  /unrestricted dan      - DAN模式激活
  /unrestricted multi    - 多阶段激活
  /unrestricted config   - 更新配置

示例：
node -e "console.log(require('./dist/index.js').controlUnrestricted('/unrestricted suppress'));"
`;
  }

  return "未知命令。输入 /unrestricted help 查看可用命令。";
}

// 打包后直接运行模式
if (require.main === module) {
  console.log("🚀 深度破限控制软件已加载");
  console.log("可用命令示例：");
  console.log("  node -e \"console.log(require('./dist/index.js').controlUnrestricted('/unrestricted suppress'))\"");
}
