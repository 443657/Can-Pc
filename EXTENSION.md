# 扩展任务：CAN 与主机双向桥接

本工程在基础作业（USB 调试探针、SWD、UART 回环、CAN1 初始化）之上实现扩展任务方向 1。主机通过 USB CDC 控制口发送 Classical CAN 标准数据帧，固件也把 CAN 收到的帧主动、持续地推送给主机；不再依赖 `CAN RX` 轮询。

## 1. 数据路径

```text
CAN 总线
  -> FDCAN1 RX FIFO（message RAM，RX 中断）
  -> can_rx_task（CanRx::read）
  -> CAN_TO_HOST（24 帧有界队列）
  -> host_tx_task（控制口唯一的 CDC IN 写者）
  -> USB CDC 控制口 -> 主机

主机
  -> USB CDC 控制口 OUT
  -> host_rx_task（64 字节 packet，按 '\n' 拼行）
  -> protocol::parse
  -> HOST_TO_CAN（8 帧有界队列）
  -> can_tx_task（CanTx::write，FDCAN TX mailbox）
  -> CAN 总线
```

对应代码：

| 位置 | 内容 |
| --- | --- |
| `src/main.rs` | 四个桥接任务、三个有界队列、计数器和 USB 收发串行化。 |
| `src/protocol.rs` | 主机协议解析、`RX` 行编码、96 字节行缓冲、帧类型。 |
| `src/board.rs` | 保持基础工程的 CAN1 配置：80 MHz FDCAN 内核时钟、1 Mbit/s、75% 采样点；使用 `ClassicCanOnly`。 |
| `src/wiring.rs` | 保持基础工程的 USB、SWD GPIO、UART7 中断缓冲配置。 |
| `host.py` | 主机侧命令、持续接收和帧边界自测。 |

## 2. 主机协议

控制口使用 ASCII 行协议，换行符 `\n` 是唯一帧边界；`\r` 由解析器的空白处理忽略。一行最大 96 字节（不含换行），超长行在遇到下一个换行时整行丢弃并回复 `ERR SYNTAX`，不会执行被截断的命令。

### 2.1 主机 -> 设备

| 行 | 说明 | 成功回复 |
| --- | --- | --- |
| `PING` | 检查控制口。 | `OK DM-MC02` |
| `SWD ID` | 读取目标 DPIDR。 | `OK DPIDR 2BA01477` |
| `TX <id> <dlc> <data>` | 发送一帧 Classical CAN 标准数据帧。 | `OK TX` |
| `TX <id> <dlc> -` | 发送 0 字节载荷（`dlc` 必须为 0）。 | `OK TX` |
| `CAN TX <id> <data>` | 兼容基础作业的旧格式，`dlc` 由数据长度推断。 | `OK TX` |
| `STAT` | 读取计数器。 | `OK rx=... tx=... drx=... dtx=...` |

字段定义：

- `<id>`：1–3 个十六进制字符，范围 `000`–`7FF`，不带 `0x`。
- `<dlc>`：十进制 `0`–`8`。
- `<data>`：恰好 `2 * dlc` 个十六进制字符；`dlc = 0` 时写 `-`。
- 只接受标准数据帧；扩展帧/远程帧若进入 RX FIFO，会被 `ClassicFrame::from_can` 拒绝并计入 `drx`；CAN FD 未启用（`ClassicCanOnly`）。
- `OK TX` 表示命令已通过解析并进入 `HOST_TO_CAN` 队列，实际总线发送由 `can_tx_task` 异步完成；发送失败或超时会计入 `dtx`。

### 2.2 设备 -> 主机

| 行 | 说明 |
| --- | --- |
| `RX <id> <dlc> <data>` | 每收到一帧 CAN 标准数据帧就主动发送一行。 |
| `OK ...` / `ERR ...` | 对主机命令的应答。 |

接收示例：

```text
RX 123 2 AABB
RX 7FF 0 -
```

发送示例：

```text
主机写：TX 123 2 AABB\n
设备回：OK TX\n
```

### 2.3 帧边界

- **一条请求分成多次到达**：`host_rx_task` 把每个 USB packet 的字节送进 `protocol::Line`，只有遇到 `\n` 才解析；未完成的行保留到下一个 packet。
- **一次收到多条请求**：同一个 packet 中的每个 `\n` 都会触发一次解析，按字节顺序执行。
- **错误行**：语法错误回复 `ERR SYNTAX`；ID、DLC 或数据格式错误回复 `ERR FRAME`；主机 -> CAN 队列满回复 `ERR BUSY`；旧 `CAN RX` 回复 `ERR STREAM`（接收已改为主动推送）。

## 3. 并发与缓冲

| 任务/路径 | 职责 | 缓冲与满队列处理 |
| --- | --- | --- |
| `can_rx_task` | 等待 FDCAN RX，转换为 `ClassicFrame`。 | `CAN_TO_HOST` 深度 24；`try_send` 失败时丢弃该帧并增加 `drx`，不阻塞 CAN 接收。 |
| `host_tx_task` | 唯一控制口 CDC IN 写者；应答优先于 CAN 帧。 | 应答队列深度 8，CAN 帧队列深度 24；USB 写失败时清空两个队列，等待下一次连接，避免重放旧帧。 |
| `host_rx_task` | 读 CDC OUT、拼行、解析并执行命令。 | 行缓冲 96 字节；`TX` 用 `try_send` 投递到深度 8 的 `HOST_TO_CAN`，满时回复 `ERR BUSY`。 |
| `can_tx_task` | 唯一 FDCAN TX 写者。 | 从 `HOST_TO_CAN` 取帧；`CanTx::write` 最多等待 250 ms，超时丢弃并增加 `dtx`，避免总线无 ACK 或 bus-off 时永久卡住。 |
| `uart_port` | 保持基础作业的 UART7 <-> 第二个 CDC 口原始字节桥。 | UART7 使用中断环形缓冲（静态 32 字节 RX/TX），与 CAN 桥独立。 |
| USB device / DAP | USB 枚举和 CMSIS-DAP 演示接口。 | 与桥接任务独立。 |

关键点：

- 所有队列都是 `static` 的 `embassy_sync::channel::Channel`，容量在编译期确定，不使用堆。
- CAN 接收路径从不在 USB 上等待：队列满就丢弃并计数，保证 CAN 中断/异步接收不被主机拖死。
- USB 写路径由单个 `host_tx_task` 串行化，避免两个任务同时写 CDC IN 造成交错。
- 应答发送有 200 ms 超时；主机不读时不会无限阻塞命令解析，丢掉的应答计入 `dtx`。
- `STAT` 的四个计数器：
  - `rx`：从 CAN 读到的总帧数；
  - `tx`：成功交给 FDCAN TX mailbox 的总帧数；
  - `drx`：CAN -> 主机方向丢弃的帧数（队列满、扩展/远程帧、断连清空）；
  - `dtx`：主机 -> CAN 方向丢弃的帧数（`ERR BUSY`、发送超时、TX mailbox 替换低优先级帧）加上被丢弃的主机应答数。

### 中断与 DMA

- FDCAN1 使用 `CanIrqs` 绑定 `FDCAN1_IT0` / `FDCAN1_IT1`，由 `CanConfigurator` 配置 message RAM；RX/TX 由中断驱动，任务只 `await` 驱动 future。
- UART7 使用 `BufferedInterruptHandler` 和静态收发缓冲区，未使用 DMA。
- USB OTG_HS 使用 `UsbIrqs` 中断。
- 本方向选择 USB 作为主机链路，因此 UART 侧不需要 DMA；如果改走 UART 路线，可在 UART 一侧增加 DMA，CAN 侧仍使用 message RAM + 中断。

## 4. 编译与检查

工具链（本机实测）：

```text
rustc 1.99.1 (58cf8f9c3 2026-10-13)
cargo 1.99.1 (5f94df478 2026-08-27)
target: thumbv7em-none-eabihf
```

命令：

```sh
cargo build --locked
python3 -m py_compile host.py
```

结果：

- `cargo build --locked` 成功，完整输出见 [`build.log`](build.log)。
- `python3 -m py_compile host.py` 退出码 0，无输出（语法检查通过）。

硬件行为说明：以上只验证了编译和主机脚本语法。FDCAN 收发、USB 持续转发、队列满/断连行为均**尚未上板验证**；下面的测试步骤是预期行为，需在具备第二个 CAN 节点和 USB 主机的条件下执行。

## 5. 上板验证步骤（预期）

1. 用第二个 1 Mbit/s Classical CAN 节点接入 CANH/CANL，总线两端各 120 Ω 终端。
2. 打开控制口，运行 `python3 host.py CONTROL_PORT can-monitor`。
3. 从第二个节点发标准帧，主机应持续看到 `RX <id> <dlc> <data>`。
4. 运行 `python3 host.py CONTROL_PORT can-send 123 AABB`，第二个节点应收到 ID `0x123`、DLC 2、数据 `AA BB`。
5. 运行 `python3 host.py CONTROL_PORT bridge-test`，验证拆包、粘包、超长行和非法帧的边界处理。
6. `python3 host.py CONTROL_PORT stat` 查看 `rx/tx/drx/dtx`；拔掉 USB 再插回，确认旧帧不会重放。

## 6. 与基础作业的兼容性

- `PING`、`SWD ID`、UART 回环和基础 CAN1 配置保持不变。
- `CAN TX <id> <data>` 旧格式仍可用。
- 旧的 `CAN RX` 轮询被主动 `RX` 流取代；收到该命令时回复 `ERR STREAM`。主机脚本的 `can-recv` 已改为等待一条流式 `RX`。

## 7. 限制

- 仅支持 Classical CAN 标准数据帧（11 位 ID，0–8 字节）；扩展帧、远程帧和 CAN FD 未实现。
- bus-off 自动恢复沿用 `embassy-stm32` 的默认配置（`automatic_bus_off_recovery = true`）；发送任务另有 250 ms 超时，避免无 ACK 的总线把任务永久卡住。
- 未做硬件在环验证；编译成功只说明可以生成固件。
