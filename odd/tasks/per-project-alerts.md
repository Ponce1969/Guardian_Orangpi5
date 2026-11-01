# Monitoreo por proyecto: alertas con identidad

Hoy el daemon vigila una sola base PostgreSQL y su estado de alerta se indexa por
`MetricKind`. Sumar `Pedidos_Multi` y `E-commerce-MVP` exige que la alerta sepa **de
qué instancia habla** y que el estado sea independiente por instancia; sin eso, la
recuperación de una base cierra la alerta de otra que sigue caída.

## Specs

**S1.** La alerta nombra el proyecto. Con tres bases vigiladas, un mensaje que dice
`postgres_probe_failures` no permite saber cuál investigar.

**S2.** El estado de alerta es independiente por instancia. Que una instancia se
recupere no debe emitir `Recovered` mientras otra siga fallando, ni silenciar el
`Firing` de una instancia distinta por coincidir en el tipo de métrica.

**S3.** Se vigilan las tres bases por health probe: `auditor_familiar` (127.0.0.1:5442,
usuario `Gallinal2218`), `barraca` (127.0.0.1:5445, usuario `barraca_user`) y
`web_2r_prod` (127.0.0.1:5446, usuario `web2r_prod_user`).

**S4.** Cada proyecto tiene su propio backup diario a las 03:30, con su propia
credencial y su propio `database`.

**S5.** Las credenciales viven en `/opt/guardianrs/.env` con el esquema
`GUARDIAN_PG_<PROYECTO>_PASSWORD`: `GUARDIAN_PG_AUDITOR_PASSWORD`,
`GUARDIAN_PG_PEDIDOS_PASSWORD`, `GUARDIAN_PG_ECOMMERCE_PASSWORD`. El nombre del
proyecto, nunca el puerto: el puerto cambia, el proyecto no.

**S6.** Una métrica sin etiquetas se comporta como hoy: una sola identidad, un solo
estado. Los colectores que no etiquetan no cambian de comportamiento.

**S7.** `scripts/verify-credentials.sh` reproduce el camino real del daemon (TCP al
host, no el socket del contenedor) y reporta por separado variable ausente, puerto
cerrado y fallo de autenticación.

## Tasks

| ID | Spec | Ruta | Qué |
| --- | --- | --- | --- |
| T1 | S1, S2, S6 | inline | Identidad de alerta: `kind` + etiquetas ordenadas, y el tracker indexa por ella |
| T2 | S1 | inline | `AlertEvent` lleva la identidad y el mensaje la incluye |
| T3 | S3 | inline | `postgres.instances` en la config y un collector por instancia en `main` |
| T4 | S5 | inline | Renombrar `GUARDIAN_PG_PASSWORD` a `GUARDIAN_PG_AUDITOR_PASSWORD` |
| T5 | S4 | inline | Tres tareas de backup, una por proyecto |
| T6 | S1-S5 | inline | Verificar en el Pi: tres probes verdes y un dump por proyecto |

## Log

**L1** (pedido original):

> hola como deberiamos seguir este proyecto quiero monitorear estos dos proyectos Pedidos_Multi , E-commerce-MVP , que nombre de variables le pondremos y guardamos en el .env con las contrsaeña de postgresql .

**L2** (corrección del usuario, identifica el proyecto):

> si es la web de ecommerce de 2 rodriguez , esa debe monitorear .

**L3** (evidencia, verificación de credenciales por el camino del daemon):

```
auditor     OK   25 tablas en auditor_familiar via GUARDIAN_PG_PASSWORD (nombre anterior)
pedidos     OK   13 tablas en barraca via GUARDIAN_PG_PEDIDOS_PASSWORD
ecommerce   OK   9 tablas en web_2r_prod via GUARDIAN_PG_ECOMMERCE_PASSWORD
```

**L4** (decisión de diseño, no pedida explícitamente): la identidad de una alerta es
`kind` más sus etiquetas ordenadas, y no una etiqueta `instance` obligatoria. Razón:
no obliga a ningún colector existente a cambiar, y una métrica sin etiquetas conserva
exactamente el comportamiento actual (S6).
