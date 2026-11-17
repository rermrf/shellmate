//! Uncoloured prompts, fed with idle pauses through the normal backend sink so
//! all four identities are learned by the same path as a real session.
pub const STEPS: &[&str] = &[
    "root@serv2147439633:~/wen/cpa# ",
    concat!(
        "docker ps\r\n",
        "CONTAINER ID   IMAGE          STATUS          NAMES\r\n",
        "b07a91c21f03   nginx:1.27     Up 3 hours      web\r\n",
        "\r\nemoji@emojideMac-mini ~ % "
    ),
    "ls\r\nApplications  Desktop  Documents  Downloads\r\n\r\n[root@centos7 ~]# ",
    "uptime\r\n 09:41:03 up 12 days,  3:18,  1 user,  load average: 0.03, 0.04, 0.00\r\n\r\n(venv) deploy@web:~/app$ ",
    concat!(
        "\r\n",
        "Output mentions root@serv2147439633:~/wen/cpa# in the middle.\r\n",
        "Server colours: the following prompt keeps magenta and cyan.\r\n",
        "\x1b[35mroot@serv2147439633\x1b[0m:\x1b[36m~/wen/cpa\x1b[0m# echo server-styled\r\n",
        "server-styled\r\n\r\nroot@serv2147439633:~/wen/cpa# "
    ),
];

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn prompt_palette_matches_prototype_green_blue_and_foreground() {
        use shellmate_term::TerminalPalette;
        for (theme, palette) in [
            ("light", TerminalPalette::light()),
            ("dark", TerminalPalette::dark()),
        ] {
            for (token, rgb) in [
                ("--ag", palette.ansi[2]),
                ("--ab", palette.ansi[4]),
                ("--tfg", palette.foreground),
            ] {
                let expected = super::super::DATA["TERM"][theme][token].s();
                assert_eq!(
                    format!("#{:02X}{:02X}{:02X}", rgb.r, rgb.g, rgb.b),
                    expected.to_uppercase()
                );
            }
        }
    }

    #[test]
    fn plain_prompt_scene_has_idle_steps_and_only_one_server_coloured_line() {
        assert!(super::super::SCENES.contains(&"prompt-plain"));
        assert_eq!(STEPS.len(), 5);
        assert!(
            STEPS[..4]
                .iter()
                .all(|s| !s.contains('\x1b') && s.ends_with(' '))
        );
        let text = STEPS.concat();
        for sample in [
            "root@serv2147439633:~/wen/cpa# docker ps",
            "emoji@emojideMac-mini ~ % ls",
            "[root@centos7 ~]# uptime",
            "(venv) deploy@web:~/app$ ",
            "Output mentions root@serv2147439633:",
            "\x1b[35mroot@serv2147439633",
        ] {
            assert!(text.contains(sample), "{sample:?}");
        }
        assert!(text.ends_with("root@serv2147439633:~/wen/cpa# "));
        assert!(!text.contains("\x1b[?25l"));
    }
}
