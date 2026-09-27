# ADR-004: Realtime protocol evolution

- Status: Accepted
- Date: 2026-07-31
- Decision Owners: avistoria

## Context

Protocol Buffers schema、code generation、subprotocol、version negotiation、互換性、debug JSON、圧縮を最初の公開contract前に固定する必要がある。

## Decision

- `proto/orbisync/v1/realtime.proto`をRealtime契約の正本とし、packageは`orbisync.v1`とする。
- production subprotocolは`orbisync.v1.protobuf`とする。
- JSON debug modeは`json-debug` featureでcompileした開発環境だけに限定し、subprotocolは`orbisync.v1.json`とする。
- code generation、lint、breaking-change検査にBufを使用する。生成codeはcommitせず、build/CIで再生成する。
- protocol major不一致は接続拒否、minorはserverが対応する最小値へnegotiationする。未知fieldは無視する。
- field numberの再利用を禁止し、削除fieldはnameとnumberを`reserved`へ移す。
- Envelope field 1〜19をheader、20以降をpayloadとして予約する。
- SnapshotはReliable bulk laneでchunk化し、latest-wins/Controlへ入れない。
- WebSocket compressionは初期無効とする。測定後にpermessage-deflateをopt-in追加できる。

## Alternatives

- JSON production protocol: 可読性は高いがsize/CPU効率と厳密な互換検査で劣る。
- generated codeのcommit: consumerには便利だが差分noiseとtoolchain不一致を生む。
- major不一致のfallback: 意味論の不一致を隠すため不採用。

## Consequences

- Buf設定とcompatibility baselineの管理が必要になる。
- JSON debugはproductionで利用できない。
- 圧縮を初期無効にすることでcompression side channelとCPU負荷を避ける。

## Migration

最初のschemaを作成し、golden binaryとinvalid vectorsを`test-vectors/protocol/v1/`へ配置する。
