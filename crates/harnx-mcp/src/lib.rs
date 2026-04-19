//! `harnx-mcp` — MCP (Model Context Protocol) support for the harnx
//! workspace. Currently exports just the safety/validation utilities
//! used by the harnx-mcp-bash and harnx-mcp-fs helper bins; in future
//! plans this crate will also hold the MCP client glue.

pub mod safety;
