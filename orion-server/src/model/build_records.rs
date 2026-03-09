use api_model::buck2::{status::Status, types::ProjectRelativePath};
use sea_orm::{ConnectionTrait, DbErr, EntityTrait};
use uuid::Uuid;

use super::{builds, orion_tasks::OrionTask};

pub async fn ensure_orion_task_record(
    db: &impl ConnectionTrait,
    task_id: Uuid,
    cl_link: &str,
    repo: &str,
    changes: &Vec<Status<ProjectRelativePath>>,
) -> Result<(), DbErr> {
    if callisto::orion_tasks::Entity::find_by_id(task_id)
        .one(db)
        .await?
        .is_none()
    {
        OrionTask::insert_task(task_id, cl_link, repo, changes, db).await?;
    }

    Ok(())
}

pub async fn ensure_build_records(
    db: &impl ConnectionTrait,
    build_id: Uuid,
    task_id: Uuid,
    target_id: Uuid,
    repo: &str,
) -> Result<(), DbErr> {
    if builds::Entity::find_by_id(build_id).one(db).await?.is_none() {
        builds::Model::insert_build(build_id, task_id, target_id, repo.to_string(), db).await?;
    }

    if callisto::build_events::Entity::find_by_id(build_id)
        .one(db)
        .await?
        .is_none()
    {
        callisto::build_events::Model::insert_build(build_id, task_id, repo.to_string(), db)
            .await?;
    }

    Ok(())
}
