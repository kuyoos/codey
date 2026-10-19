// D:\Desktop\codey\codey_unrestricted.ts
// 深度破限插件集成入口 - 支持控制台开关 + 保存到全局配置

import { UnrestrictedPlugin } from "./unrestricted_plugin";

export const unrestrictedPlugin = new UnrestrictedPlugin();

// 统一命令处理器（已支持控制台开关命令）
export function handleUnrestrictedCommand(command: string) {
  switch (command.toLowerCase()) {
    case "toggle":
    case "on":
    case "off":
      return unrestrictedPlugin.toggleEnabled();
    
    case "dan":
    case "multi":
    case "suppress":
      return unrestrictedPlugin.activateDeepUnrestricted();
    
    case "status":
      return JSON.stringify(unrestrictedPlugin.getStatus(), null, 2);
    
    case "config":
      console.log("配置已更新");
      return "配置已更新";
    
    default:
      return "可用命令：/unrestricted [toggle|on|off|dan|multi|suppress|status|config]";
  }
}
