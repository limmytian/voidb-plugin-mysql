# VoidB MySQL Plugin (`voidb-plugin-mysql`)

[![License](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)

Independent process plugin for [VoidB](https://github.com/limmytian/voidb) to connect, inspect, and manage MySQL and MariaDB databases.

## Features

- **Autonomous Process Architecture**: Runs in an isolated OS process communicating with VoidB via `stdio-jsonrpc`.
- **Tri-Modal Operation**:
  - `test`: Automated connectivity verification.
  - `serve`: `stdio-jsonrpc` protocol server for AI Agents and automated orchestration.
  - Domain commands: SQL execution, database cataloging, table inspection, and schema description.

## Usage & Operational Modes

```bash
# Connectivity test
voidb-cli mysql test --profile my-mysql

# Run SQL queries and catalog exploration
voidb-cli mysql databases --profile my-mysql
voidb-cli mysql tables --database mydb --profile my-mysql
voidb-cli mysql describe users --database mydb --profile my-mysql
voidb-cli mysql query "SELECT * FROM users LIMIT 10;" --database mydb --profile my-mysql

# Machine / Agent stdio-jsonrpc server
voidb-cli mysql serve
```
