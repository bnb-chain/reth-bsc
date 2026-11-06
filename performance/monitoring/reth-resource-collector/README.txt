reth-resource-collector（只读资源统计）
每5秒采集/proc，每60秒读取本机6060已有metrics。自动跟随唯一reth-bsc node PID，限量轮转日志。
不抓栈、不扫描DB、不启动交易；只记录白名单参数，不记录完整环境或密码。
详细字段定义、解释限制、线上配置及验证见：
performance/reports/2026-10-28-prewarm-observability-review.txt

本地测试：PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s performance/monitoring/reth-resource-collector -v
远端服务名：reth-resource-collector.service
数据目录：/server/reth2.0/diagnostics/reth-resource-collector/data/
已安装服务的部署证据在工作区performance/monitoring/2026-10-28-resident-resources/，不在Git中存服务器日志。
