//! Call templates the client renders for each time tool.
//!
//! Only call templates: `invoke` answers with plain JSON rather than MCP
//! content blocks, so the client's generic renderer formats the result.

pub(crate) const GET_CURRENT_TIME_CALL: &str =
    "🕐 time{% if args.timezone %} ({{ args.timezone }}){% endif %}";

pub(crate) const CONVERT_TIME_CALL: &str = "🕐 convert{% if args.isoTimestamp %} {{ args.isoTimestamp }}{% endif %}{% if args.unixTimestamp %} unix={{ args.unixTimestamp }}{% endif %}{% if args.epochMillis %} ms={{ args.epochMillis }}{% endif %}{% if args.sourceTimezone %} from={{ args.sourceTimezone }}{% endif %}{% if args.offsetDays %} +{{ args.offsetDays }}d{% endif %}{% if args.offsetHours %} +{{ args.offsetHours }}h{% endif %}{% if args.offsetMinutes %} +{{ args.offsetMinutes }}m{% endif %}{% if args.offsetSeconds %} +{{ args.offsetSeconds }}s{% endif %}{% if args.timezone %} → {{ args.timezone }}{% endif %}";

pub(crate) const WAIT_CALL: &str = "⏳ wait {{ args.seconds }}s";

pub(crate) const WAIT_UNTIL_CALL: &str =
    "⏳ wait until {{ args.time }}{% if args.timezone %} ({{ args.timezone }}){% endif %}";
