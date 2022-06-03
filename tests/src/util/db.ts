import * as sqlite from "sqlite"
import sqlite3 from "sqlite3"

export class DroneDatabase {
  private constructor(private db: sqlite.Database) {}

  static async create(path: string): Promise<DroneDatabase> {
    const db = await sqlite.open({
      filename: path,
      driver: sqlite3.Database,
    })
    return new DroneDatabase(db)
  }

  async addProxy(backend: string, address: string): Promise<void> {
    await this.db.run(
      `
                insert into route
                (backend, address)
                values
                (?, ?)
                `,
      backend,
      address
    )
  }

  async getLastActiveTime(backend: string): Promise<number | null> {
    const row = await this.db.get(
      `
            select last_active
            from route
            where backend = ?
            `,
      backend
    )

    return row["last_active"]
  }
}
