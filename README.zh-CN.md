# Polypulse

[English](./README.md) | **中文**

**官方网站**：[polypulse.wiki](https://polypulse.wiki/)

> 面向 [Polymarket](https://polymarket.com) 加密货币「Up or Down」5 分钟预测市场的 Rust 套利机器人。

![account](image/account.png)

![运行界面](image/main.png)

## 基本原理

Polymarket 的 Up/Down 市场每 5 分钟（UTC）开一个新窗口，每个市场有 YES 和 NO 两个结果代币。

理论上，持有等量 YES + NO 可在结算时兑换 1 USDC，因此：

```
YES 卖一价 + NO 卖一价 < 1  →  存在套利空间
```

机器人大致流程如下：

1. **发现市场** — 按配置的币种（如 btc、eth）自动查找当前 5 分钟窗口对应的 Up/Down 市场。
2. **监控订单簿** — 订阅 CLOB 订单簿，实时计算 YES + NO 的合计价格。
3. **执行套利** — 当合计价格低于阈值时，同时买入 YES 和 NO；可通过滑点、单笔上限、执行价差等参数控制下单行为。
4. **Merge 回收** — 若同时持有 YES 和 NO，可链上 Merge 合并为 USDC/pUSD，减少持仓风险。
5. **窗口收尾** — 接近窗口结束时，可自动取消挂单、Merge、并卖出剩余单腿仓位。

> 本程序连接真实市场与真实资金，使用前请充分理解风险。

## 快速开始

### 预编译可执行文件

如果你不会编译代码，请直接使用 **[Releases](https://github.com/crazygirl437/Polymarket-5min-bot/releases/tag/V.10)** 中提供的预编译可执行文件：

1. 下载对应系统（Linux / Windows）的安装包
2. 复制 `.env.example` 为 `.env`，并填写必填项
3. 运行：
   - Linux / macOS：`./polypulse`
   - Windows：`polypulse.exe`

### 从源码编译

需要先安装 [Rust](https://rustup.rs)。

```bash
cp .env.example .env   # 填写 .env 配置文件的必填项后启动
cargo run              # 运行程序
```

详细参数说明见 `.env.example`，按 `[1]` ~ `[9]` 分区排列：越靠前越重要。

## 基础配置

### 必填

| 变量 | 说明 |
|------|------|
| `POLYMARKET_PRIVATE_KEY` | 签名私钥。邮箱/Magic 从 [reveal.magic.link/polymarket](https://reveal.magic.link/polymarket) 导出；浏览器钱包导出对应 EOA 私钥 |
| `POLYMARKET_PROXY_ADDRESS` | 资金托管地址（Settings 里的 Address，非 EOA），见 [polymarket.com/settings](https://polymarket.com/settings) |

### 签名类型 `SIGNATURE_TYPE`

按 **Settings 里资金钱包的类型** 选择，与「邮箱还是浏览器钱包注册」无必然对应：

| 值 | 适用场景 |
|----|----------|
| `Poly1271`（**默认**） | V2 deposit wallet；邮箱/Magic 与浏览器钱包授权账号均适用 |
| `Proxy` | 仅 V1 旧 Magic 代理（Settings 地址须等于 ProxyFactory 从 EOA 推导的地址） |
| `GnosisSafe` | Gnosis Safe 多签 |
| `Eoa` | 纯 EOA 直连，无需 `POLYMARKET_PROXY_ADDRESS` |

**判断方法：** 保持默认 `Poly1271` 即可。若误设 `Proxy` 且下单报 `please use the deposit wallet flow`，说明账号已走 V2 deposit wallet，应改回 `Poly1271`（私钥与 `POLYMARKET_PROXY_ADDRESS` 无需改动）。

### Merge 所需（启用定时 Merge 或收尾时必填）

| 变量 | 说明 |
|------|------|
| `POLY_BUILDER_API_KEY` | Builder API Key |
| `POLY_BUILDER_SECRET` | Builder API Secret |
| `POLY_BUILDER_PASSPHRASE` | Builder API Passphrase |

以上三项在 Polymarket → Settings → Builder 获取。

### 常用可选项

| 变量 | 默认值 | 说明 |
|------|--------|------|
| `CRYPTO_SYMBOLS` | `btc,eth,sol,xrp` | 监控的币种，逗号分隔 |
| `ARBITRAGE_EXECUTION_SPREAD` | `0.01` | 执行阈值：`yes + no <= 1 - spread` 时下单 |
| `MAX_ORDER_SIZE_USDC` | `100.0` | 单笔最大下单量 |
| `RISK_MAX_EXPOSURE_USDC` | `1000.0` | 每轮最大风险敞口 |
| `MERGE_INTERVAL_MINUTES` | `0` | 定时 Merge 间隔（分钟），`0` 为关闭 |
| `WIND_DOWN_BEFORE_WINDOW_END_MINUTES` | `0` | 窗口结束前收尾（分钟），`0` 为关闭 |
| `RUST_LOG` | `info` | 日志级别 |

其余参数（CLOB 地址、签名类型、滑点、订单类型、持仓同步等）均有合理默认值，一般无需修改。完整列表与注释见 `.env.example`。

## 免责声明

本软件仅供学习与研究，不构成任何投资建议。加密货币与预测市场存在较高风险，可能导致资金损失。使用前请自行评估风险，并遵守 Polymarket 服务条款及当地法律法规。
