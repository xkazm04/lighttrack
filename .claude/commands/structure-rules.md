# Project Structure Rules (FastAPI)

**CRITICAL**: All code changes must follow these structure rules.

## Directory Structure

### Core Directories
- `app/` - All application code
- `app/main.py` - FastAPI app instance and startup
- `app/api/` - API route handlers (routers)
- `app/models/` - Database models and Pydantic schemas
- `app/services/` - Business logic layer
- `app/core/` - Configuration, settings, security
- `app/db/` - Database connection and session management
- `app/crud/` - Database CRUD operations

### File Placement Rules

**1. Separation of Concerns**
- Routes: `app/api/[resource].py` - Handle HTTP requests/responses only
- Services: `app/services/[resource]_service.py` - Business logic
- CRUD: `app/crud/[resource].py` - Database operations
- Models: `app/models/[resource].py` - Data models

**2. Configuration**
- All config in `app/core/config.py` using Pydantic BaseSettings
- Security utilities in `app/core/security.py`
- Dependencies in `app/core/deps.py`

**3. Database**
- Connection management in `app/db/database.py`
- Session handling in `app/db/session.py`
- Models use SQLAlchemy or your chosen ORM

**4. API Routes**
- Each router handles one resource
- Group related endpoints in one file
- Use dependency injection for shared logic

## Anti-Patterns (DO NOT USE)

❌ `app/utils/**` - Use `app/core/` or `app/services/`
❌ `app/helpers/**` - Use `app/core/` or `app/services/`
❌ `*.py` in root - Keep all code under `app/`
❌ Mixed concerns in routes - Separate routes, services, and data access

## Before Adding/Moving Files

1. **Is it a route handler?** → `app/api/`
2. **Is it business logic?** → `app/services/`
3. **Is it database access?** → `app/crud/`
4. **Is it configuration?** → `app/core/`
5. **Is it a data model?** → `app/models/`

## Examples

✅ Good:
- `app/api/users.py - User routes`
- `app/services/user_service.py - User business logic`
- `app/crud/user.py - User CRUD operations`
- `app/models/user.py - User models`
- `app/core/config.py - Application config`

❌ Bad:
- `app/utils/user_helper.py - Should be `app/services/user_service.py``
- `user_routes.py in root - Should be `app/api/users.py``
- `Business logic in route handlers - Should be in services`

## Enforcement

Before creating or moving any file, verify it follows these rules. Use Structure Scan to detect violations.
