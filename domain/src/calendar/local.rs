//! The computer's own time zone, by its IANA name. A repeating event with
//! no zone repeats in it, and so does one whose zone name nothing here
//! reads, since the readers place its first occurrence there.

use std::path::Path;
use std::sync::OnceLock;

use chrono_tz::Tz;

/// The computer's zone, read once on first use. UTC when nothing names a
/// zone this build knows.
pub fn zone() -> Tz {
    static ZONE: OnceLock<Tz> = OnceLock::new();
    *ZONE.get_or_init(|| {
        let link = std::fs::read_link("/etc/localtime").ok();
        let file = std::fs::read_to_string("/etc/timezone").ok();
        let zone = from_system(std::env::var("TZ").ok().as_deref(), link.as_deref(), file.as_deref());
        tracing::debug!(zone = zone.name(), "the computer's time zone");
        zone
    })
}

/// The zone the `TZ` variable, the `/etc/localtime` link target and the
/// `/etc/timezone` text name, tried in that order. `TZ` goes first because
/// chrono, glibc and GLib let it win over `/etc/localtime`, and the views
/// draw local times through them.
pub(crate) fn from_system(tz: Option<&str>, localtime: Option<&Path>, timezone: Option<&str>) -> Tz {
    tz.and_then(|value| known(value.strip_prefix(':').unwrap_or(value)))
        .or_else(|| localtime.and_then(from_link))
        .or_else(|| timezone.and_then(known))
        .unwrap_or(Tz::UTC)
}

/// The zone a link into a zoneinfo folder names: Linux's
/// `/usr/share/zoneinfo/Europe/Lisbon`, Flatpak's relative
/// `../usr/share/zoneinfo/...`, or macOS's
/// `/var/db/timezone/zoneinfo/...`.
fn from_link(target: &Path) -> Option<Tz> {
    let text = target.to_str()?;
    let (_, name) = text.rsplit_once("zoneinfo/")?;
    let name = name.strip_prefix("posix/").or_else(|| name.strip_prefix("right/")).unwrap_or(name);
    known(name)
}

/// The zone `name` names, when it is an IANA name this build knows. A
/// POSIX rule such as `WET0WEST,M3.5.0/1,M10.5.0` names none.
fn known(name: &str) -> Option<Tz> {
    name.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono_tz::America::New_York;
    use chrono_tz::Europe::Lisbon;

    #[test]
    fn the_localtime_link_names_the_zone() {
        let link = Path::new("/usr/share/zoneinfo/Europe/Lisbon");
        assert_eq!(from_system(None, Some(link), None), Lisbon);
    }

    #[test]
    fn a_relative_or_posix_link_names_the_zone() {
        let link = Path::new("../usr/share/zoneinfo/posix/Europe/Lisbon");
        assert_eq!(from_system(None, Some(link), None), Lisbon);
    }

    #[test]
    fn the_macos_link_names_the_zone() {
        let link = Path::new("/var/db/timezone/zoneinfo/America/New_York");
        assert_eq!(from_system(None, Some(link), None), New_York);
    }

    #[test]
    fn the_tz_variable_wins_over_the_link() {
        let link = Path::new("/usr/share/zoneinfo/Europe/Lisbon");
        assert_eq!(from_system(Some(":America/New_York"), Some(link), None), New_York);
    }

    #[test]
    fn a_posix_rule_in_tz_falls_through_to_the_link() {
        let link = Path::new("/usr/share/zoneinfo/Europe/Lisbon");
        assert_eq!(from_system(Some("EST5EDT,M3.2.0,M11.1.0"), Some(link), None), Lisbon);
    }

    #[test]
    fn the_timezone_file_names_the_zone_when_nothing_else_does() {
        assert_eq!(from_system(None, Some(Path::new("/etc/localtime.real")), Some("Europe/Lisbon\n")), Lisbon);
    }

    #[test]
    fn nothing_readable_means_utc() {
        assert_eq!(from_system(Some(""), None, Some("garbage")), Tz::UTC);
    }
}
