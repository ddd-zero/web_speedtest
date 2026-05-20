# Debian systemd 守护服务

本目录提供一份备用的 `web_speedtest.service`。它不会自动安装或启动，只用于部署到 Debian 服务器时复制到 systemd。

## 约定路径

- 二进制：`/opt/web_speedtest/web_speedtest`
- 配置文件：`/etc/web_speedtest/config.toml`
- 工作目录：`/var/lib/web_speedtest`
- 默认数据库：如果 `config.toml` 里仍使用相对路径 `speed_results.sqlite3`，文件会落在 `/var/lib/web_speedtest/speed_results.sqlite3`
- 可选环境覆盖：`/etc/default/web_speedtest`

## 安装示例

```bash
sudo useradd --system --home-dir /var/lib/web_speedtest --shell /usr/sbin/nologin web_speedtest

sudo install -d -o root -g root -m 0755 /opt/web_speedtest
sudo install -d -o root -g root -m 0755 /etc/web_speedtest
sudo install -d -o web_speedtest -g web_speedtest -m 0750 /var/lib/web_speedtest

sudo install -m 0755 web_speedtest /opt/web_speedtest/web_speedtest
sudo install -m 0644 config.toml /etc/web_speedtest/config.toml
sudo install -m 0644 deploy/systemd/web_speedtest.service /etc/systemd/system/web_speedtest.service

sudo systemctl daemon-reload
sudo systemctl enable --now web_speedtest.service
```

## 常用命令

```bash
sudo systemctl status web_speedtest.service
sudo journalctl -u web_speedtest.service -f
sudo systemctl restart web_speedtest.service
```

## 可选环境文件

如需临时切换配置路径，可以创建 `/etc/default/web_speedtest`：

```ini
WEB_SPEEDTEST_CONFIG=/etc/web_speedtest/config.toml
```

`EnvironmentFile=-/etc/default/web_speedtest` 中的短横线表示文件不存在时不阻止服务启动。
