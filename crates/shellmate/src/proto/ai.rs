//! Static answer sources shared by the real drawer and headless fixture checks.
pub const AGENT_ANALYSIS: &str = r#"检查完成，容器服务运行正常。

- **运行状态：** 5 个容器均在运行，使用 `docker ps` 已确认。
- **入口日志：** `nginx` 最近请求正常，没有发现 `5xx`。
- **内部服务：** API、数据库和缓存通过内部网络提供服务。

| 名称 | 镜像 | 端口 |
| --- | --- | --- |
| nginx | nginx:1.27-alpine | 443/tcp |
| api | shellmate/api:2.8.1 | 8080/tcp |
| worker | shellmate/jobs:2.8.1 | — |
| postgres | postgres:16-alpine | 5432/tcp |
| redis | redis:7-alpine | 6379/tcp |

如需继续查看资源占用：

```sh
docker stats --no-stream
```"#;

pub const CHAT_ANALYSIS: &str = "不会，这是一条只读查询命令。\n\n1. `lsof` 只列出监听 `3000` 端口的进程及 PID。\n2. 先预览命令，再确认目标会话为 `prod-web-01`。\n3. 将结果发给我后，可以继续分析端口冲突。";

pub const MARKDOWN: &str = r#"## 服务检查结果

当前服务**运行正常**，建议*持续观察*；可查阅 [Docker 文档](https://docs.docker.com/)。

### 接下来检查什么

- 入口服务 `nginx`
  - 检查最近的访问日志
  - 确认 `443` 端口与证书有效期
- 内部服务
  - 数据库 `postgres` 与缓存 `redis`

> 以下命令只读取状态。执行前请确认当前会话和目标容器。

```sh
docker inspect nginx --format '{{.Name}} {{.Config.Image}} {{json .NetworkSettings.Ports}} {{json .NetworkSettings.Networks}} {{json .Mounts}}'
```

| 名称 | 镜像版本 | 端口映射 | 挂载目录 | 健康状态 |
| --- | --- | --- | --- | --- |
| production-nginx-gateway | registry.example.com/production/nginx:1.27-alpine | 0.0.0.0:443->443/tcp | /srv/production/nginx/configuration | 正常，连续运行 12 天 |
| production-api-service | registry.example.com/production/shellmate-api:2.8.1 | 127.0.0.1:8080->8080/tcp | /srv/production/api/application-data | 正常，连续运行 3 天 |

![容器服务拓扑](https://example.invalid/never-fetch.png)
"#;

#[cfg(test)]
mod tests {
    use markdown::{ParseOptions, mdast::Node};

    fn walk<'a>(node: &'a Node, nodes: &mut Vec<&'a Node>) {
        nodes.push(node);
        if let Some(children) = node.children() {
            for child in children {
                walk(child, nodes);
            }
        }
    }

    #[test]
    fn ai_markdown_fixture_covers_requested_constructs() {
        let root = markdown::to_mdast(super::MARKDOWN, &ParseOptions::gfm()).unwrap();
        let mut nodes = Vec::new();
        walk(&root, &mut nodes);
        for depth in [2, 3] {
            assert!(
                nodes
                    .iter()
                    .any(|n| matches!(n, Node::Heading(h) if h.depth == depth))
            );
        }
        assert!(nodes.iter().any(|n| matches!(n, Node::Strong(_))));
        assert!(nodes.iter().any(|n| matches!(n, Node::Emphasis(_))));
        assert!(nodes.iter().any(|n| matches!(n, Node::Link(_))));
        assert!(nodes.iter().any(|n| matches!(n, Node::Blockquote(_))));
        assert!(nodes.iter().any(|n| matches!(n, Node::Image(_))));
        assert!(
            nodes
                .iter()
                .any(|n| matches!(n, Node::Code(c) if c.value.len() > 120))
        );
        assert!(
            nodes
                .iter()
                .any(|n| matches!(n, Node::Table(t) if t.align.len() == 5))
        );
        assert!(nodes.iter().filter(|n| matches!(n, Node::List(_))).count() >= 3);
        let agent = markdown::to_mdast(super::AGENT_ANALYSIS, &ParseOptions::gfm()).unwrap();
        nodes.clear();
        walk(&agent, &mut nodes);
        assert!(
            nodes
                .iter()
                .any(|n| matches!(n, Node::Table(t) if t.align.len() == 3))
        );
        let chat = markdown::to_mdast(super::CHAT_ANALYSIS, &ParseOptions::gfm()).unwrap();
        nodes.clear();
        walk(&chat, &mut nodes);
        assert!(
            nodes
                .iter()
                .any(|n| matches!(n, Node::List(l) if l.ordered))
        );
        assert!(nodes.iter().any(|n| matches!(n, Node::InlineCode(_))));
    }

    #[test]
    fn ai_fixtures_have_both_themes_and_requested_sizes() {
        for scene in ["ai", "ai-chat", "ai-agent-run", "ai-markdown"] {
            assert!(super::super::SCENES.contains(&scene));
            for dark in [false, true] {
                assert!(
                    super::super::readings::READINGS
                        .iter()
                        .any(|r| r.scene == scene && r.dark == dark)
                );
            }
            for (w, h) in [(1440, 900), (1100, 700)] {
                let request = format!("{scene}@{w}x{h}");
                assert_eq!(super::super::request(&request), (scene, Some((w, h))));
            }
        }
    }
}
